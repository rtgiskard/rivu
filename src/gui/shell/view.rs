use super::*;
use rivu_ui::{
    Dialog, IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, Popover, caption,
    column, menu_item, row,
};

impl Render for WindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = rivu_ui::theme::palette(cx);
        self.presentation.update(cx, |presentation, cx| {
            presentation.request_frame(window, cx)
        });
        self.synchronize_interaction(window, cx);
        self.synchronize_keyboard_focus(window, cx);
        if self.workspace.interaction.docking().is_some()
            && (!cx.has_active_drag() || self.panel_docking_blocked(cx))
        {
            self.cancel_panel_docking(window, cx);
        }
        window.set_rem_size(px(16. * self.state.system.config.ui_scale));
        self.workspace
            .scroll_views
            .borrow_mut()
            .begin_frame(self.workspace.layouts.current());
        let docking_view = cx.entity().downgrade();
        let paint_presentation = self.presentation.clone();
        let paint_artwork_images = self.artwork_images.clone();
        let layouts = self.workspace.layouts.clone();
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
            .font_size_scale(rivu_ui::theme::font_size_scale(cx))
            .font(self.ui_font.clone())
            .track_focus(self.shell.layout_focus())
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let surface = this.current_keyboard_surface(cx);
                if surface.is_none() {
                    this.shell.capture_origin(window, cx);
                }
                if let Some(surface) = surface {
                    let within_surface = match surface {
                        KeyboardSurface::Settings => this
                            .settings
                            .read(cx)
                            .focus_handle(cx)
                            .contains_focused(window, cx),
                        _ => this.shell.scope().handle().contains_focused(window, cx),
                    };
                    if !within_surface {
                        this.focus_keyboard_surface(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                if event.keystroke.key == "escape"
                    && event.keystroke.modifiers == gpui::Modifiers::default()
                    && (this.workspace.interaction.docking().is_some()
                        || this.workspace.interaction.dragging().is_some()
                        || this.workspace.interaction.seek_preview().is_some()
                        || this.audio_processing.read(cx).pointer_active()
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
                let modal = this.current_keyboard_surface(cx).is_some();
                if !modal {
                    this.shell.capture_origin(window, cx);
                }
                this.workspace
                    .scroll_views
                    .borrow_mut()
                    .click(event.position, modal);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.current_keyboard_surface(cx).is_none() {
                        this.shell.capture_origin(window, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.synchronize_interaction(window, cx);
                this.drag_position(event.position, event.modifiers.alt, cx);
            }))
            .on_drag_move::<PanelDrag>(cx.listener(
                |this, event: &DragMoveEvent<PanelDrag>, window, cx| {
                    let source = event.drag(cx).source;
                    if this
                        .workspace
                        .interaction
                        .docking()
                        .is_none_or(|session| session.source != source)
                    {
                        this.start_panel_docking(source, window, cx);
                    }
                    this.update_panel_docking(window.mouse_position(), window, cx);
                },
            ))
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, window, cx| {
                if event.button == MouseButton::Left {
                    // A disabled child may stop Bubble, but cannot own the end of a
                    // window gesture. Let normal drop/seek handlers run first.
                    let docking_source = this
                        .workspace
                        .interaction
                        .docking()
                        .map(|session| session.source);
                    let entity = cx.entity().downgrade();
                    window.defer(cx, move |window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.synchronize_interaction(window, cx);
                            if let Some(source) = docking_source
                                && this
                                    .workspace
                                    .interaction
                                    .docking()
                                    .is_some_and(|session| session.source == source)
                            {
                                // Revalidate and commit through the same path as on_drop.
                                this.drop_panel_docking(source, window, cx);
                            }
                            this.finish_drag(cx);
                            let cancelled = this
                                .waveform
                                .update(cx, |waveform, _| waveform.cancel_unreleased_seek());
                            if let Some(generation) = cancelled
                                && this.workspace.interaction.cancel_waveform(generation)
                            {
                                cx.notify();
                            }
                        });
                    });
                }
            }))
            .on_drop(cx.listener(|this, drag: &PanelDrag, window, cx| {
                this.drop_panel_docking(drag.source, window, cx);
                cx.stop_propagation();
            }))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    this.synchronize_interaction(window, cx);
                    if let Some(Dragging::Split(drag)) = this.workspace.interaction.dragging() {
                        this.drag_position(drag.pointer, event.modifiers.alt, cx);
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.synchronize_interaction(window, cx);
                    this.workspace.interaction.cancel_docking();
                    this.finish_drag(cx);
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        let entity = docking_view.clone();
                        window.on_mouse_event::<MouseExitEvent>(move |_, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = entity
                                    .update(cx, |this, cx| this.cancel_panel_docking(window, cx));
                            }
                        });
                        paint_presentation
                            .update(cx, |presentation, cx| presentation.finish_paint(cx));
                        paint_artwork_images.release_retired(window);
                    },
                )
                .absolute()
                .size_full(),
            );

        let mut footer = row().gap_0().relative().flex_shrink_0().text_xs();
        if let Some(notification) = self.footer_notification(cx) {
            footer = footer.child(notification);
        }
        footer = footer.child(
            div()
                .id("footer-window-drag")
                .flex_1()
                .min_w_16()
                .self_stretch()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if this.current_keyboard_surface(cx).is_none()
                            && this.workspace.interaction.dragging().is_none()
                            && this.workspace.interaction.docking().is_none()
                            && !cx.has_active_drag()
                        {
                            window.start_window_move();
                            cx.stop_propagation();
                        }
                    }),
                ),
        );
        let toggle_layout_lock = cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
            let cancellation = this.workspace.interaction.lock_layout();
            if cancellation.docking {
                cx.stop_active_drag(window);
            }
            cancellation.restore_split(this.workspace.layouts.current_mut());
            cx.stop_propagation();
            this.workspace.layouts.current_mut().locked = !this.workspace.layouts.current().locked;
            this.shell.dismiss(shell::Surface::PanelCatalog);
            this.panel_menu = None;
            this.reconcile_feature_demand(cx);
            cx.notify();
        });
        let tools = IconButtonGroup::new(IconButtonStyle::Quiet, IconButtonDensity::Compact)
            .button_with(
                IconButton::new(
                    "layout",
                    if self.workspace.layouts.current().locked {
                        "󰌾"
                    } else {
                        ""
                    },
                    if self.workspace.layouts.current().locked {
                        self.tr("shell-layout-locked-hint")
                    } else {
                        self.tr("shell-layout-unlocked-hint")
                    },
                )
                .selected(self.shell.is_open(shell::Surface::PanelCatalog)),
                cx,
                |this, _, cx| {
                    if this.workspace.layouts.current().locked {
                        return;
                    }
                    this.shell.toggle(shell::Surface::PanelCatalog);
                    this.panel_menu = None;
                    cx.notify();
                },
                |button| button.on_mouse_down(MouseButton::Right, toggle_layout_lock),
            )
            .button(
                IconButton::new("app-menu", "", self.tr("shell-menu"))
                    .selected(self.shell.is_open(shell::Surface::AppMenu)),
                cx,
                |this, _, cx| {
                    this.shell.toggle(shell::Surface::AppMenu);
                    this.panel_menu = None;
                    cx.notify();
                },
            )
            .into_div();
        footer = footer.child(tools);
        let mut footer_popup = None;
        if self.shell.is_open(shell::Surface::AppMenu) {
            let about = menu_item("app-about", self.tr("shell-about"), cx, |this, _, cx| {
                this.shell.open(shell::Surface::About);
                cx.notify();
            });
            footer_popup = Some(
                Popover::menu(theme, "app-menu-popup", window)
                    .min_w(rems(8.))
                    .map(|surface| self.menu_surface(surface, cx))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.shell.dismiss(shell::Surface::AppMenu);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(menu_item(
                        "app-settings",
                        self.tr("shell-settings"),
                        cx,
                        |this, window, cx| {
                            this.settings
                                .update(cx, |settings, cx| settings.open(window, cx));
                            this.shell.open(shell::Surface::Settings);
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
                            this.shell.open(shell::Surface::Layouts);
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
                                this.shell.dismiss(shell::Surface::AppMenu);
                                if let Err(error) = this.handle.update_gui_config(
                                    crate::core::GuiConfigChange::ToggleVisualization,
                                ) {
                                    this.error = Some(format!("{error:#}"));
                                }
                                cx.notify();
                            },
                        )
                        .child(
                            rivu_ui::icon(
                                if self.state.system.config.visualization_enabled {
                                    "✓"
                                } else {
                                    ""
                                },
                                cx,
                            )
                            .ml_auto()
                            .size(rems(1.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(rems(0.875))
                            .line_height(relative(1.))
                            .text_color(rgb(
                                if self.state.system.config.visualization_enabled {
                                    theme.accent
                                } else {
                                    theme.muted
                                },
                            )),
                        ),
                    )
                    .child(rivu_ui::menu_separator(theme))
                    .child(menu_item(
                        "app-keyboard-help",
                        self.tr("shell-shortcuts"),
                        cx,
                        |this, window, cx| {
                            this.shell.dismiss(shell::Surface::AppMenu);
                            this.open_keyboard_help(window, cx);
                        },
                    ))
                    .child(about)
                    .child(rivu_ui::menu_separator(theme))
                    .child(menu_item(
                        "app-close-window",
                        self.tr("shell-close-window"),
                        cx,
                        |this, window, cx| {
                            this.shell.dismiss(shell::Surface::AppMenu);
                            this.request_close_window(window, cx);
                        },
                    ))
                    .child(menu_item(
                        "app-quit",
                        self.tr("shell-quit"),
                        cx,
                        |this, _, cx| {
                            this.shell.dismiss(shell::Surface::AppMenu);
                            this.request_quit(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        if self.shell.is_open(shell::Surface::PanelCatalog)
            && !self.workspace.layouts.current().locked
        {
            footer_popup = Some(self.render_panel_catalog(theme, cx));
        }

        if let Some(popup) = footer_popup {
            // Absolute content reads the footer's intrinsic size without sizing it.
            footer = footer.child(
                gpui::container_query(move |size, window, _| {
                    let outer_inset = window.rem_size() * 0.25;
                    let inset = window.rem_size() * 0.5;
                    div().relative().size_full().child(
                        popup
                            .absolute()
                            .bottom(size.height + outer_inset)
                            .right(window.rem_size() - outer_inset)
                            .max_w(
                                (window.viewport_size().width - window.rem_size() - inset)
                                    .max(px(0.)),
                            )
                            .max_h(
                                (window.viewport_size().height
                                    - size.height
                                    - outer_inset * 2.
                                    - inset)
                                    .max(px(0.)),
                            )
                            .overflow_y_scroll(),
                    )
                })
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        }
        app = app.child(
            column()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .p_1()
                .gap_1()
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(layout_view)
                        .child(self.measurement(Measured::Workspace)),
                )
                .child(footer),
        );
        if self.workspace.interaction.docking().is_some() {
            app = app.child(self.docking_overlay(theme));
        }

        let menu_scope = matches!(
            self.current_keyboard_surface(cx),
            Some(KeyboardSurface::Menu(..))
        )
        .then(|| self.shell.scope().clone());
        let dialog_scope = (!self.manual_confirmation_open()).then(|| self.shell.scope().clone());
        let layout_focus = self.shell.layout_focus().clone();
        self.playlists.update(cx, |playlists, cx| {
            playlists.set_shell_context(layout_focus, menu_scope, dialog_scope, cx);
        });
        for surface in self.surface_layers(cx) {
            match surface {
                KeyboardSurface::Menu(MenuSurface::App | MenuSurface::PanelCatalog) => {}
                KeyboardSurface::Menu(MenuSurface::Panel(_)) => {
                    let state = self.panel_menu.expect("live panel menu");
                    app = app.child(self.render_panel_menu(state, theme, window, cx));
                }
                KeyboardSurface::Menu(MenuSurface::Content(_)) => {
                    app = app.child(self.content_menu_overlay(window, cx));
                }
                KeyboardSurface::Menu(MenuSurface::Playlist)
                | KeyboardSurface::Dialog(DialogSurface::PlaylistDelete) => {
                    app = app.child(self.playlists.update(cx, |playlists, cx| {
                        playlists.playlist_action_overlay(window, cx)
                    }));
                }
                KeyboardSurface::Dialog(DialogSurface::QueueReplacement) => {
                    app = app.child(self.queue_replace_overlay(window, cx));
                }
                KeyboardSurface::Dialog(DialogSurface::PlaylistEditor) => {
                    app = app.child(self.playlists.update(cx, |playlists, cx| {
                        playlists.playlist_name_dialog(window, cx)
                    }));
                }
                KeyboardSurface::Dialog(
                    DialogSurface::LayoutManager | DialogSurface::LayoutAction,
                ) => {
                    app = app.child(self.layouts_panel(window, cx));
                }
                KeyboardSurface::Settings => {
                    app = app.child(
                        self.settings
                            .clone()
                            .cached(gpui::StyleRefinement::default().absolute().size_full()),
                    );
                }
                KeyboardSurface::Dialog(DialogSurface::DuplicateReview) => {
                    let surface = self.dialog_surface(Dialog::backdrop("duplicate-review-modal"));
                    app = app.child(self.duplicates.update(cx, |duplicates, cx| {
                        duplicates
                            .render_modal(surface, window, cx)
                            .into_any_element()
                    }));
                }
                KeyboardSurface::Dialog(DialogSurface::About) => {
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
                Dialog::backdrop("about-modal")
                    .map(|surface| self.dialog_surface(surface))
                    .child(
                        Dialog::scroll_card(theme, "about-card", window)
                            .flex_row()
                            .flex_wrap()
                            .relative()
                            .items_center()
                            .gap_6()
                            .p_12()
                            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                this.shell.dismiss(shell::Surface::About);
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
                                            .gap_2()
                                            .child(caption(theme, self.tr("shell-tagline")))
                                            .child(
                                                column().gap_1().children(
                                                    [
                                                        (
                                                            self.tr("shell-version"),
                                                            self.i18n.format(
                                                                "shell-version-features",
                                                                &[
                                                                    (
                                                                        "version",
                                                                        env!("RIVU_GIT_VERSION")
                                                                            .into(),
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
                                                            .gap_2()
                                                            .child(
                                                                div()
                                                                    .w_16()
                                                                    .flex_shrink_0()
                                                                    .child(caption(theme, label)),
                                                            )
                                                            .child(caption(theme, value))
                                                    }),
                                                ),
                                            ),
                                    ),
                            )
                            .child(
                                IconButton::new("about-close", "", self.tr("shell-close-about"))
                                    .build(
                                        IconButtonStyle::Quiet,
                                        IconButtonDensity::Regular,
                                        cx,
                                        |this, window, cx| {
                                            this.shell.dismiss(shell::Surface::About);
                                            this.focus_layout(window, cx);
                                            cx.notify();
                                        },
                                    )
                                    .absolute()
                                    .top(px(8.))
                                    .right(px(8.)),
                            ),
                    ),
            );
                }
                KeyboardSurface::Dialog(DialogSurface::KeyboardHelp) => {
                    let context = self.shell.help().expect("live keyboard help");
                    app = app.child(self.keyboard_help_overlay(context, cx));
                }
                KeyboardSurface::Dialog(DialogSurface::MetadataConfirmation) => {
                    if let Some(confirmation) = self.render_manual_confirmation(window, cx) {
                        app = app.child(confirmation);
                    }
                }
            }
        }
        self.workspace.scroll_views.borrow_mut().finish_frame();
        app
    }
}
