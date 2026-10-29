use crate::gui::window::*;
use rivu_ui::{
    FormField, IconButton, IconButtonDensity, IconButtonStyle, StatusKind, button, caption, column,
    copyable_message, menu_item, row, status_message,
};

#[derive(Clone)]
pub(in crate::gui) enum Action {
    Duplicate,
    Rename,
    Delete(String),
}

impl WindowView {
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
                    self.focus_keyboard_surface(window, cx);
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
        let mut content = rivu_ui::Dialog::scroll_card(theme, "layouts-content", window)
            .p_3()
            .gap_2()
            .child(
                rivu_ui::panel_heading_row(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_lg()
                        .child(self.tr("shell-layouts")),
                    rivu_ui::panel_header_actions().child(
                        IconButton::new("layouts-close", "", self.tr("shell-close-layouts"))
                            .build(
                                IconButtonStyle::Quiet,
                                IconButtonDensity::Regular,
                                cx,
                                |this, window, cx| {
                                    this.shell.dismiss(shell::Surface::Layouts);
                                    this.layout_action = None;
                                    this.focus_layout(window, cx);
                                    cx.notify();
                                },
                            )
                            .focus(|style| {
                                style
                                    .border_color(gpui::transparent_black())
                                    .text_color(rgb(theme.accent))
                            }),
                    ),
                )
                .pb_2()
                .border_b_1()
                .border_color(rgb(theme.border)),
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
            content = content.child(caption(theme, title).px_2());
            if let Action::Delete(name) = &action {
                content = content.child(div().px_2().text_sm().child(self.i18n.format(
                    "shell-delete-layout-confirm",
                    &[("name", name.as_str().into())],
                )));
            } else {
                content = content.child(
                    FormField::new(theme, self.tr("shell-name"), self.layout_name.clone())
                        .into_div()
                        .px_2(),
                );
            }
            content = content.child(
                row()
                    .flex_wrap()
                    .justify_end()
                    .px_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(theme.border))
                    .child(
                        IconButton::new(
                            "layout-action-cancel",
                            "",
                            self.tr("shell-cancel-layout-action"),
                        )
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, window, cx| {
                                this.layout_action = None;
                                this.focus_keyboard_surface(window, cx);
                                cx.notify();
                            },
                        ),
                    )
                    .child(if matches!(action, Action::Delete(_)) {
                        rivu_ui::danger_control(
                            theme,
                            button("layout-action-submit", submit, cx, |this, window, cx| {
                                this.finish_layout_action(window, cx)
                            }),
                        )
                    } else {
                        IconButton::new("layout-action-submit", "", submit).build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
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
                let target: SharedString = name.to_owned().into();
                let mut choice = row()
                    .id(target.clone())
                    .group("layout-choice-row")
                    .w_full()
                    .min_h_8()
                    .gap_1()
                    .child(
                        menu_item(
                            ("layout-select", index),
                            target.clone(),
                            cx,
                            move |this, window, cx| this.select_layout(&target, window, cx),
                        )
                        .flex_1()
                        .min_w_0(),
                    );
                if current {
                    choice = choice.child(
                        div()
                            .size_8()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(theme.accent))
                            .child("✓"),
                    );
                } else {
                    let name = name.to_owned();
                    choice = choice.child(rivu_ui::hover_reveal(
                        ("layout-delete-reveal", index),
                        "layout-choice-row",
                        IconButton::new(
                            ("layout-delete", index),
                            "󰆴",
                            self.i18n.format(
                                "shell-delete-layout-hint",
                                &[("name", name.as_str().into())],
                            ),
                        )
                        .build(
                            IconButtonStyle::Quiet,
                            IconButtonDensity::Regular,
                            cx,
                            move |this, window, cx| {
                                this.begin_layout_action(Action::Delete(name.clone()), window, cx)
                            },
                        )
                        .flex_shrink_0(),
                    ));
                }
                choices = choices.child(choice);
            }
            content = content
                .child(
                    div()
                        .id("layout-list")
                        .pr_2()
                        .max_h((window.viewport_size().height * 0.5).min(px(360.)))
                        .overflow_y_scroll()
                        .child(choices),
                )
                .child(
                    row()
                        .flex_wrap()
                        .justify_between()
                        .px_2()
                        .pt_2()
                        .border_t_1()
                        .border_color(rgb(theme.border))
                        .child(
                            row()
                                .flex_shrink_0()
                                .child(
                                    IconButton::new(
                                        "layout-duplicate",
                                        "",
                                        self.tr("shell-duplicate-layout-as"),
                                    )
                                    .build(
                                        IconButtonStyle::Surface,
                                        IconButtonDensity::Regular,
                                        cx,
                                        |this, window, cx| {
                                            this.begin_layout_action(Action::Duplicate, window, cx)
                                        },
                                    ),
                                )
                                .child(
                                    IconButton::new(
                                        "layout-rename",
                                        "󰏫",
                                        self.tr("shell-rename-layout"),
                                    )
                                    .build(
                                        IconButtonStyle::Surface,
                                        IconButtonDensity::Regular,
                                        cx,
                                        |this, window, cx| {
                                            this.begin_layout_action(Action::Rename, window, cx)
                                        },
                                    ),
                                ),
                        )
                        .child(
                            IconButton::new("layout-sync", "󱈖", self.tr("shell-sync-layouts"))
                                .build(
                                    IconButtonStyle::Surface,
                                    IconButtonDensity::Regular,
                                    cx,
                                    |this, window, cx| {
                                        this.cancel_interaction(window, cx);
                                        this.workspace.request_save();
                                        cx.notify();
                                    },
                                ),
                        ),
                );
        }
        match self.workspace.save_status {
            workspace::SaveStatus::Saving => {
                content = content.child(
                    status_message(theme, self.tr("shell-layouts-syncing"), StatusKind::Info)
                        .px_2(),
                );
            }
            workspace::SaveStatus::Finished if self.workspace.save_error.is_none() => {
                content = content.child(
                    status_message(theme, self.tr("shell-layouts-synced"), StatusKind::Success)
                        .px_2(),
                );
            }
            _ => {}
        }
        if let Some(error) = &self.error {
            content = content.child(
                copyable_message(
                    "layout-management-error",
                    self.tr("shell-copy-message"),
                    error.clone(),
                    cx,
                )
                .text_color(rgb(theme.error))
                .px_2(),
            );
        }
        rivu_ui::Dialog::backdrop("layouts-modal")
            .map(|surface| self.dialog_surface(surface))
            .child(content.w(px(460.)))
            .into_any_element()
    }
}

impl WindowView {
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
