//! Root input, keyboard-surface routing, and top-level composition.

use super::*;
use crate::gui::components::{
    ButtonTooltip, caption, column, copyable_message, icon_button, menu_item, row,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardSurface {
    Menu(u8, u8),
    Dialog(u8),
    Settings,
}

impl GuiApp {
    pub(super) fn menu_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if matches!(
            self.current_keyboard_surface(),
            Some(KeyboardSurface::Menu(..))
        ) {
            self.keyboard_scope.menu_surface(surface)
        } else {
            surface
        }
    }

    pub(super) fn dialog_surface(&self, surface: gpui::Stateful<Div>) -> gpui::Stateful<Div> {
        if self.manual_confirmation_open() {
            surface
        } else {
            self.keyboard_scope.surface(surface)
        }
    }

    fn current_keyboard_surface(&self) -> Option<KeyboardSurface> {
        if self.keyboard_help.is_some() {
            return Some(KeyboardSurface::Dialog(7));
        }
        if self.manual_confirmation_open() {
            return Some(KeyboardSurface::Dialog(0));
        }
        if self.about_open {
            return Some(KeyboardSurface::Dialog(1));
        }
        if self.duplicate_review.open {
            return Some(KeyboardSurface::Dialog(2));
        }
        if self.settings_open {
            return Some(KeyboardSurface::Settings);
        }
        if self.playlist_editor.is_some() {
            return Some(KeyboardSurface::Dialog(3));
        }
        if let Some(action) = &self.playlist_action {
            return Some(if matches!(action, playlists::PlaylistAction::Menu(..)) {
                KeyboardSurface::Menu(0, 0)
            } else {
                KeyboardSurface::Dialog(4)
            });
        }
        if self.queue_replace.is_some() {
            return Some(KeyboardSurface::Dialog(8));
        }
        if self.layouts_open {
            return Some(KeyboardSurface::Dialog(if self.layout_action.is_some() {
                6
            } else {
                5
            }));
        }
        if let Some(menu) = &self.content_menu {
            return Some(KeyboardSurface::Menu(1, menu.keyboard_page()));
        }
        if let Some(menu) = &self.panel_menu {
            return Some(KeyboardSurface::Menu(3, menu.page as u8));
        }
        if self.app_menu_open {
            return Some(KeyboardSurface::Menu(4, 0));
        }
        if self.catalog_open {
            return Some(KeyboardSurface::Menu(5, 0));
        }
        None
    }

    fn synchronize_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let surface = self.current_keyboard_surface();
        if surface == self.keyboard_surface {
            return;
        }
        if self.keyboard_surface.is_none() && surface.is_some() && self.keyboard_restore.is_none() {
            self.keyboard_restore = window.focused(cx);
        }
        self.keyboard_surface = surface;
        if let Some(surface) = surface {
            if surface != KeyboardSurface::Settings {
                let scope = self.keyboard_scope.clone();
                let entity = cx.weak_entity();
                window.on_next_frame(move |window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if this.keyboard_surface == Some(surface)
                            && (matches!(surface, KeyboardSurface::Menu(..))
                                || scope.handle().is_focused(window)
                                || !scope.handle().contains_focused(window, cx))
                        {
                            scope.handle().focus(window, cx);
                            if !matches!(surface, KeyboardSurface::Menu(..)) {
                                scope.advance(false, window, cx);
                            }
                        }
                    });
                });
            }
        } else if let Some(origin) = self.keyboard_restore.take() {
            // A removed trigger's handle is no longer in the dispatch tree; fall back to the layout.
            if self.layout_focus.contains(&origin, window) {
                origin.focus(window, cx);
            } else {
                self.focus_layout(window, cx);
            }
        }
    }

    pub(super) fn menu_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            "down" | "up" => self.keyboard_scope.advance(key == "up", window, cx),
            "escape" | "left" => {
                if self.content_menu.is_some() {
                    self.back_content_menu(cx);
                } else if let Some(menu) = &mut self.panel_menu {
                    if matches!(menu.page, PanelMenuPage::Main) {
                        self.panel_menu = None;
                    } else {
                        menu.page = PanelMenuPage::Main;
                    }
                } else if self.playlist_action.is_some() {
                    self.playlist_action = None;
                } else {
                    self.app_menu_open = false;
                    self.catalog_open = false;
                }
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        self.sync_loading_text(cx);
        if self.analysis_frame_interval().is_some()
            && !self.frame_pending
            && self.frame_timer.is_none()
        {
            self.request_frame(window, cx);
        }
        self.synchronize_interaction(window, cx);
        self.synchronize_keyboard_focus(window, cx);
        if self.interaction.docking().is_some()
            && (!cx.has_active_drag() || self.panel_docking_blocked())
        {
            self.cancel_panel_docking(window, cx);
        }
        window.set_rem_size(px(16. * self.state.system.config.ui_scale));
        self.scroll_views.begin_frame();
        self.visuals.begin_frame();
        let docking_view = cx.entity().downgrade();
        let paint_visuals = self.visuals.clone();
        let paint_visualizer = self.audio_visualizer.clone();
        let paint_waveform = self.waveform.read(cx).image_releaser();
        let paint_artwork_images = self.artwork_images.clone();
        let layouts = self.layouts.clone();
        let layout_view = layouts
            .current()
            .root
            .as_ref()
            .map(|root| self.render_node(root, window, cx))
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(theme.muted))
                    .child(self.tr("shell-empty-layout"))
                    .into_any_element()
            });
        let mut app = column()
            .id("rivu")
            .relative()
            .size_full()
            .gap_0()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text))
            .text_size(px(14. * self.state.system.config.ui_scale))
            .font(self.ui_font.clone())
            .track_focus(&self.layout_focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.current_keyboard_surface().is_none() {
                    this.keyboard_restore = window.focused(cx);
                }
                if event.keystroke.key == "escape"
                    && event.keystroke.modifiers == gpui::Modifiers::default()
                    && (this.interaction.docking().is_some()
                        || this.interaction.dragging().is_some()
                        || this.interaction.seek_preview().is_some()
                        || cx.has_active_drag())
                {
                    this.cancel_interaction(window, cx);
                    cx.stop_active_drag(window);
                    cx.stop_propagation();
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "f4"
                    && modifiers.alt
                    && !modifiers.control
                    && !modifiers.platform
                    && !modifiers.shift
                    && !modifiers.function
                {
                    this.request_close_window(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(Self::keyboard_input))
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                if this.current_keyboard_surface().is_none() {
                    this.keyboard_restore = window.focused(cx);
                }
                let modal = this.manual_confirmation_open()
                    || this.settings_open
                    || this.about_open
                    || this.keyboard_help.is_some()
                    || this.duplicate_review.open
                    || this.playlist_editor.is_some()
                    || this.playlist_action.is_some()
                    || this.queue_replace.is_some()
                    || this.app_menu_open
                    || this.panel_menu.is_some()
                    || this.content_menu.is_some()
                    || this.catalog_open;
                this.scroll_views.click(event.position, modal);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.current_keyboard_surface().is_none() {
                        this.keyboard_restore = window.focused(cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.synchronize_interaction(window, cx);
                this.drag_position(event.position, event.modifiers.alt, cx);
            }))
            .on_drag_move::<PanelDrag>(cx.listener(
                |this, event: &DragMoveEvent<PanelDrag>, window, cx| {
                    let source = event.drag(cx).panel_id;
                    if this
                        .interaction
                        .docking()
                        .is_none_or(|session| session.source != source)
                    {
                        this.start_panel_docking(source, window, cx);
                    }
                    this.update_panel_docking(window.mouse_position(), window, cx);
                },
            ))
            .capture_any_mouse_up(cx.listener(|_, event: &MouseUpEvent, window, cx| {
                if event.button == MouseButton::Left {
                    let entity = cx.entity().downgrade();
                    window.defer(cx, move |_, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            if this.interaction.cancel_docking() {
                                cx.notify();
                            }
                            let cancelled = this
                                .waveform
                                .update(cx, |waveform, _| waveform.cancel_unreleased_seek());
                            if let Some(generation) = cancelled
                                && this.interaction.cancel_waveform(generation)
                            {
                                cx.notify();
                            }
                        });
                    });
                }
            }))
            .on_drop(cx.listener(|this, drag: &PanelDrag, window, cx| {
                this.drop_panel_docking(drag.panel_id, window, cx);
                cx.stop_propagation();
            }))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    this.synchronize_interaction(window, cx);
                    if let Some(Dragging::Split(drag)) = this.interaction.dragging() {
                        this.drag_position(drag.pointer, event.modifiers.alt, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.synchronize_interaction(window, cx);
                    this.finish_drag(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.synchronize_interaction(window, cx);
                    this.interaction.cancel_docking();
                    this.finish_drag(cx);
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let entity = docking_view.clone();
                        window.on_mouse_event::<MouseExitEvent>(move |_, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = entity
                                    .update(cx, |this, cx| this.cancel_panel_docking(window, cx));
                            }
                        });
                        paint_visuals.release_retired(window);
                        paint_waveform.release_retired(window);
                        paint_visualizer.release_retired_images(window);
                        paint_artwork_images.release_retired(window);
                        paint_visuals.finish_prepaint();
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p(gpui::px(UI_INSET))
                    .child(layout_view),
            );

        let error = self
            .error
            .clone()
            .or_else(|| self.state.system.last_error.clone());
        let footer_height = window.rem_size() * 2.25 + px(UI_INSET);
        let mut footer = row()
            .gap_0()
            .h(footer_height)
            .flex_shrink_0()
            .px(gpui::px(UI_INSET))
            .pt_1()
            .pb(gpui::px(UI_INSET))
            .text_xs()
            .text_color(rgb(theme.muted));
        if let Some(error) = error {
            let message: SharedString = error.clone().into();
            let copied_message = message.clone();
            footer = footer.child(
                row()
                    .flex_initial()
                    .min_w_0()
                    .gap_3()
                    .px_2()
                    .bg(rgb(theme.error_bg))
                    .text_color(rgb(theme.text))
                    .child(
                        div()
                            .id("error-message-text")
                            .flex_initial()
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .child(message.clone())
                            .tooltip(move |_, cx| {
                                cx.new(|_| ButtonTooltip {
                                    text: message.clone(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copied_message.to_string(),
                                ));
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        row()
                            .flex_shrink_0()
                            .gap_0()
                            .child(
                                super::components::compact_icon_button(
                                    theme,
                                    icon_button(
                                        "copy-error",
                                        "",
                                        self.tr("shell-copy-message"),
                                        cx,
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                error.clone(),
                                            ));
                                        },
                                    ),
                                )
                                .group_hover("control", |style| {
                                    style.bg(rgb(theme.text).alpha(0.08))
                                }),
                            )
                            .child(
                                super::components::compact_icon_button(
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
                                )
                                .group_hover("control", |style| {
                                    style.bg(rgb(theme.text).alpha(0.08))
                                }),
                            ),
                    ),
            );
        } else if self.state.system.scanning {
            footer = footer.child(
                copyable_message(
                    "copy-scan-message",
                    self.state.system.scan_message.clone(),
                    cx,
                )
                .text_color(rgb(theme.text)),
            );
        }
        footer = footer
            .child(div().flex_1().min_w(gpui::rems(4.)))
            .child(
                icon_button(
                    "layout",
                    if self.layouts.current().locked {
                        "󰌾"
                    } else {
                        ""
                    },
                    if self.layouts.current().locked {
                        self.tr("shell-layout-locked-hint")
                    } else {
                        self.tr("shell-layout-unlocked-hint")
                    },
                    cx,
                    |this, _, cx| {
                        if this.layouts.current().locked {
                            return;
                        }
                        this.catalog_open = !this.catalog_open;
                        this.panel_menu = None;
                        cx.notify();
                    },
                )
                .mr_2()
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, window, cx| {
                        let cancellation = this.interaction.lock_layout();
                        if cancellation.docking {
                            cx.stop_active_drag(window);
                        }
                        cancellation.restore_split(this.layouts.current_mut());
                        cx.stop_propagation();
                        this.layouts.current_mut().locked = !this.layouts.current().locked;
                        this.catalog_open = false;
                        this.panel_menu = None;
                        cx.notify();
                    }),
                ),
            )
            .child(icon_button(
                "app-menu",
                "",
                self.tr("shell-menu"),
                cx,
                |this, _, cx| {
                    this.app_menu_open = !this.app_menu_open;
                    this.catalog_open = false;
                    this.panel_menu = None;
                    cx.notify();
                },
            ));
        if self.app_menu_open {
            let about = menu_item("app-about", self.tr("shell-about"), cx, |this, _, cx| {
                this.app_menu_open = false;
                this.about_open = true;
                cx.notify();
            });
            app = app.child(
                components::floating_surface(theme)
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .min_h_0()
                    .id("app-menu-popup")
                    .map(|surface| self.menu_surface(surface))
                    .occlude()
                    .absolute()
                    .bottom(footer_height)
                    .right(gpui::px(UI_INSET))
                    .w(rems(220. / 16.))
                    .max_w((window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.)))
                    .max_h(
                        (window.viewport_size().height - footer_height - px(UI_INSET)).max(px(0.)),
                    )
                    .overflow_y_scroll()
                    .p_1()
                    .gap_1()
                    .rounded_md()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.app_menu_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(menu_item(
                        "app-settings",
                        self.tr("shell-settings"),
                        cx,
                        |this, window, cx| {
                            this.app_menu_open = false;
                            this.settings
                                .update(cx, |settings, cx| settings.open(window, cx));
                            this.settings_open = true;
                            cx.notify();
                        },
                    ))
                    .child(menu_item(
                        "app-layouts",
                        self.tr("shell-layouts"),
                        cx,
                        |this, window, cx| {
                            if !this.layout_management_allowed(cx) {
                                return;
                            }
                            this.app_menu_open = false;
                            this.layouts_open = true;
                            this.layout_action = None;
                            this.focus_layout(window, cx);
                            cx.notify();
                        },
                    ))
                    .child(
                        menu_item(
                            "app-visualizations",
                            self.tr("shell-visualizations"),
                            cx,
                            |this, _, cx| {
                                this.app_menu_open = false;
                                if let Err(error) = this.handle.update_gui_config(
                                    crate::core::GuiConfigChange::ToggleStillMode,
                                ) {
                                    this.error = Some(format!("{error:#}"));
                                }
                                cx.notify();
                            },
                        )
                        .child(
                            div()
                                .ml_auto()
                                .text_color(rgb(if !self.state.system.config.still_mode {
                                    theme.accent
                                } else {
                                    theme.muted
                                }))
                                .child(if !self.state.system.config.still_mode {
                                    "✓"
                                } else {
                                    ""
                                }),
                        ),
                    )
                    .child(components::menu_separator(theme))
                    .child(menu_item(
                        "app-keyboard-help",
                        self.tr("shell-shortcuts"),
                        cx,
                        |this, window, cx| {
                            this.app_menu_open = false;
                            this.open_keyboard_help(window, cx);
                        },
                    ))
                    .child(about)
                    .child(components::menu_separator(theme))
                    .child(menu_item(
                        "app-close-window",
                        self.tr("shell-close-window"),
                        cx,
                        |this, window, cx| {
                            this.app_menu_open = false;
                            this.request_close_window(window, cx);
                        },
                    ))
                    .child(menu_item(
                        "app-quit",
                        self.tr("shell-quit"),
                        cx,
                        |this, _, cx| {
                            this.app_menu_open = false;
                            this.request_quit(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        app = app.child(footer);
        if self.interaction.docking().is_some() {
            app = app.child(self.docking_overlay(theme));
        }

        if self.catalog_open && !self.layouts.current().locked {
            app = app.child(self.render_panel_catalog(theme, footer_height, cx));
        }

        if let Some(state) = self.panel_menu {
            app = app.child(self.render_panel_menu(state, theme, window, cx));
        }
        app = app.child(self.content_menu_overlay(window, cx));

        if self.playlist_action.is_some() {
            app = app.child(self.playlist_action_overlay(window, cx));
        }
        if self.queue_replace.is_some() {
            app = app.child(self.queue_replace_overlay(window, cx));
        }
        if self.playlist_editor.is_some() {
            app = app.child(self.playlist_name_dialog(window, cx));
        }
        if self.layouts_open {
            app = app.child(self.layouts_panel(window, cx));
        }
        if self.settings_open {
            app = app.child(
                self.settings
                    .clone()
                    .cached(gpui::StyleRefinement::default().absolute().size_full()),
            );
        }
        if self.duplicate_review.open {
            app = app.child(self.duplicate_review_dialog(window, cx));
        }
        if self.about_open {
            static LOGO: std::sync::LazyLock<Arc<Image>> = std::sync::LazyLock::new(|| {
                Arc::new(Image::from_bytes(
                    ImageFormat::Png,
                    crate::artwork::DEFAULT_IMAGE.to_vec(),
                ))
            });
            let built = chrono::DateTime::<chrono::Utc>::from_timestamp(
                env!("RIVU_BUILD_TIMESTAMP")
                    .parse()
                    .expect("build script emits Unix seconds"),
                0,
            )
            .expect("build script validates timestamp range")
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
            app = app.child(
                crate::gui::components::modal_backdrop("about-modal")
                    .map(|surface| self.dialog_surface(surface))
                    .child(
                        row()
                            .id("about-card")
                            .relative()
                            .max_w(relative(0.9))
                            .items_center()
                            .gap_6()
                            .p_12()
                            .bg(rgb(theme.panel))
                            .border_1()
                            .border_color(rgb(theme.border))
                            .rounded_md()
                            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                this.about_open = false;
                                this.focus_layout(window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(
                                img(Arc::clone(&LOGO))
                                    .w(rems(9.5))
                                    .h(rems(9.5))
                                    .flex_shrink_0(),
                            )
                            .child(
                                column()
                                    .items_start()
                                    .gap_3()
                                    .child(div().text_2xl().child("Rivu"))
                                    .child(
                                        column()
                                            .gap(rems(1.))
                                            .child(caption(theme, self.tr("shell-tagline")))
                                            .children(
                                                [
                                                    (
                                                        self.tr("shell-version"),
                                                        self.i18n.format(
                                                            "shell-version-features",
                                                            &[
                                                                (
                                                                    "version",
                                                                    env!("RIVU_GIT_VERSION").into(),
                                                                ),
                                                                (
                                                                    "features",
                                                                    env!("RIVU_BUILD_FEATURES")
                                                                        .into(),
                                                                ),
                                                            ],
                                                        ),
                                                    ),
                                                    (self.tr("shell-built"), built),
                                                    (
                                                        self.tr("shell-license"),
                                                        "GPL-3.0-or-later".to_owned(),
                                                    ),
                                                ]
                                                .into_iter()
                                                .map(|(label, value)| {
                                                    row()
                                                        .gap_3()
                                                        .child(
                                                            div()
                                                                .w(rems(4.))
                                                                .flex_shrink_0()
                                                                .child(caption(theme, label)),
                                                        )
                                                        .child(caption(theme, value))
                                                }),
                                            ),
                                    ),
                            )
                            .child(
                                components::quiet_icon_button(
                                    theme,
                                    icon_button(
                                        "about-close",
                                        "",
                                        self.tr("shell-close-about"),
                                        cx,
                                        |this, window, cx| {
                                            this.about_open = false;
                                            this.focus_layout(window, cx);
                                            cx.notify();
                                        },
                                    ),
                                )
                                .absolute()
                                .top(px(8.))
                                .right(px(8.)),
                            ),
                    ),
            );
        }
        if let Some(context) = self.keyboard_help {
            app = app.child(self.keyboard_help_overlay(context, cx));
        }
        if let Some(confirmation) = self.render_manual_confirmation(cx) {
            app = app.child(confirmation);
        }
        self.scroll_views.finish_frame();
        app
    }
}
