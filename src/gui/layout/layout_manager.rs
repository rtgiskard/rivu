use super::super::*;
use rivu_ui::{button, caption, column, copyable_message, form_field, icon_button, menu_item, row};

#[derive(Clone)]
pub(in crate::gui) enum Action {
    Duplicate,
    Rename,
    Delete(String),
}

impl GuiApp {
    pub(in crate::gui) fn layout_management_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        if self.shell.is_open(shell::Surface::Settings) {
            self.error = Some(self.i18n.text("shell-settings-before-layout").to_owned());
            cx.notify();
            false
        } else {
            true
        }
    }

    fn finish_layout_switch(
        &mut self,
        cancellation: Cancellation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Workspace has committed the tree and viewport transition; feature
        // drafts and accepted metadata writes remain with their existing owners.
        if cancellation.docking {
            cx.stop_active_drag(window);
        }
        self.apply_layout_change(cancellation, cx);
        self.invalidate_manual_intent(cx);
        self.shell.dismiss(shell::Surface::Layouts);
        self.layout_action = None;
        self.shell.dismiss(shell::Surface::PanelCatalog);
        self.focus_layout(window, cx);
        self.sync_analysis(cx);
        cx.notify();
    }

    fn select_layout(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.layout_management_allowed(cx) || name == self.workspace.layouts.active_name() {
            return;
        }
        match self.workspace.activate(
            name,
            AudioVisualizerOptions::from(self.state.system.config.as_ref()),
            crate::config::AlbumArtworkOptions::from(self.state.system.config.as_ref()),
        ) {
            Ok(Some(cancellation)) => self.finish_layout_switch(cancellation, window, cx),
            Ok(None) => {}
            Err(error) => {
                self.error = Some(self.i18n.format(
                    "shell-switch-layout-error",
                    &[("error", format!("{error:#}").into())],
                ));
                cx.notify();
            }
        }
    }

    fn begin_layout_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if !self.layout_management_allowed(cx) {
            return;
        }
        let name = if matches!(action, Action::Rename) {
            self.workspace.layouts.active_name().to_owned()
        } else {
            String::new()
        };
        self.layout_name
            .update(cx, |input, cx| input.set_text(name, cx));
        if !matches!(action, Action::Delete(_)) {
            self.layout_name.read(cx).focus_handle(cx).focus(window, cx);
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
        let name = self.layout_name.read(cx).text().trim().to_owned();
        let result = match action {
            Action::Duplicate => self
                .workspace
                .duplicate(
                    &name,
                    AudioVisualizerOptions::from(self.state.system.config.as_ref()),
                    crate::config::AlbumArtworkOptions::from(self.state.system.config.as_ref()),
                )
                .map(Some),
            Action::Rename => self.workspace.layouts.rename(&name).map(|()| None),
            Action::Delete(name) => self.workspace.layouts.remove(&name).map(|()| None),
        };
        match result {
            Ok(cancellation) => {
                self.layout_action = None;
                if self.error != self.workspace.save_error {
                    self.error = None;
                }
                if let Some(cancellation) = cancellation {
                    self.finish_layout_switch(cancellation, window, cx);
                } else {
                    self.workspace.submit_save();
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
        let theme = rivu_ui::theme::palette(cx);
        let mut content = column().gap_2().child(
            row()
                .child(div().flex_1().text_lg().child(self.tr("shell-layouts")))
                .child(
                    rivu_ui::quiet_icon_button(
                        theme,
                        icon_button(
                            "layouts-close",
                            "",
                            self.tr("shell-close-layouts"),
                            cx,
                            |this, window, cx| {
                                this.shell.dismiss(shell::Surface::Layouts);
                                this.layout_action = None;
                                this.focus_layout(window, cx);
                                cx.notify();
                            },
                        ),
                    )
                    .focus(|style| {
                        style
                            .border_color(gpui::transparent_black())
                            .text_color(rgb(theme.accent))
                    }),
                ),
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
            content = content.child(caption(theme, title));
            if let Action::Delete(name) = &action {
                content = content.child(div().text_sm().child(self.i18n.format(
                    "shell-delete-layout-confirm",
                    &[("name", name.as_str().into())],
                )));
            } else {
                content = content.child(form_field(
                    theme,
                    self.tr("shell-name"),
                    self.layout_name.clone(),
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
                        rivu_ui::danger_control(
                            theme,
                            button("layout-action-submit", submit, cx, |this, window, cx| {
                                this.finish_layout_action(window, cx)
                            }),
                        )
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
            let names = self
                .workspace
                .layouts
                .names()
                .filter(|name| *name == "Default")
                .chain(
                    self.workspace
                        .layouts
                        .names()
                        .filter(|name| *name != "Default"),
                );
            for (index, name) in names.enumerate() {
                let current = name == self.workspace.layouts.active_name();
                let target = name.to_owned();
                let mut choice = row().group("layout-choice-row").child(
                    menu_item(
                        ("layout-select", index),
                        name.to_owned(),
                        cx,
                        move |this, window, cx| this.select_layout(&target, window, cx),
                    )
                    .flex_1(),
                );
                if current {
                    choice = choice.child(
                        div()
                            .size(gpui::rems(2.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(theme.accent))
                            .child("✓"),
                    );
                } else {
                    let name = name.to_owned();
                    choice = choice.child(
                        rivu_ui::quiet_icon_button(
                            theme,
                            icon_button(
                                ("layout-delete", index),
                                "󰆴",
                                self.i18n.format(
                                    "shell-delete-layout-hint",
                                    &[("name", name.as_str().into())],
                                ),
                                cx,
                                move |this, window, cx| {
                                    this.begin_layout_action(
                                        Action::Delete(name.clone()),
                                        window,
                                        cx,
                                    )
                                },
                            ),
                        )
                        .flex_shrink_0()
                        .opacity(0.)
                        .group_hover("layout-choice-row", |style| style.opacity(1.))
                        .focus(|style| style.opacity(1.))
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
                        ))
                        .child(icon_button(
                            "layout-sync",
                            "󱈖",
                            self.tr("shell-sync-layouts"),
                            cx,
                            |this, window, cx| {
                                this.cancel_interaction(window, cx);
                                this.workspace.request_save();
                                cx.notify();
                            },
                        )),
                );
        }
        if let Some(error) = &self.error {
            content = content.child(
                copyable_message("layout-management-error", error.clone(), cx)
                    .text_color(rgb(theme.error)),
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
                    .id("layouts-content")
                    .w(px(460.))
                    .max_w(relative(0.9))
                    .max_h(window.viewport_size().height * 0.9)
                    .overflow_y_scroll()
                    .p(px(UI_INSET))
                    .bg(rgb(theme.panel))
                    .border_1()
                    .border_color(rgb(theme.border))
                    .rounded_md(),
            )
            .into_any_element()
    }
}

impl GuiApp {
    pub(in crate::gui) fn sync_layout_save(&mut self, cx: &mut Context<Self>) {
        let Some(completion) = self.workspace.take_save_completion() else {
            return;
        };
        let result = completion.map_err(|error| {
            self.i18n
                .format("shell-save-layout-error", &[("error", error.into())])
        });
        match &result {
            Ok(()) => {
                if self
                    .workspace
                    .save_error
                    .as_ref()
                    .is_some_and(|error| self.error.as_ref() == Some(error))
                {
                    self.error = None;
                }
                self.workspace.save_error = None;
            }
            Err(error) => {
                self.workspace.save_error = Some(error.clone());
                self.error = Some(error.clone());
            }
        }
        cx.notify();
    }

    pub(in crate::gui) fn persist_layout(&mut self, cx: &mut Context<Self>) {
        self.workspace.persist(
            AudioVisualizerOptions::from(self.state.system.config.as_ref()),
            crate::config::AlbumArtworkOptions::from(self.state.system.config.as_ref()),
        );
        self.reconcile_feature_demand(cx);
        self.sync_analysis(cx);
        cx.notify();
    }
}
