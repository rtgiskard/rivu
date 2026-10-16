//! Top-level composition and menus; panel content remains in its existing modules.

use super::*;
use crate::{config::AudioVisualizerStyle, model::playback_key_command};

#[derive(Clone, Copy)]
pub(super) enum PanelMenuPage {
    Main,
    AudioVisualizerStyle,
    AlbumArtworkStyle,
    ConfirmClearQueue,
    ConfirmRemoveLibrary,
    Playlists,
    LibraryColumns,
}

#[derive(Clone, Copy)]
pub(super) struct PanelMenu {
    pub(super) panel_id: u64,
    pub(super) tab_bar_visible: bool,
    pub(super) position: Point<Pixels>,
    pub(super) page: PanelMenuPage,
}

impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.analysis_frame_interval().is_some()
            && !self.frame_pending
            && self.frame_timer.is_none()
        {
            self.request_frame(window, cx);
        }
        self.synchronize_interaction(window, cx);
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
        let root = self.layouts.current_mut().root.take();
        let layout_view = root
            .as_ref()
            .map(|root| self.render_node(root, window, cx))
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(MUTED))
                    .child(self.tr("shell-empty-layout"))
                    .into_any_element()
            });
        self.layouts.current_mut().root = root;
        let mut app = column()
            .id("rivu")
            .relative()
            .size_full()
            .gap_0()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(14. * self.state.system.config.ui_scale))
            .font(self.ui_font.clone())
            .track_focus(&self.layout_focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.manual_confirmation_open() {
                    if event.keystroke.key == "escape" {
                        this.cancel_manual_action(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape"
                    && (this.interaction.docking().is_some()
                        || this.interaction.dragging().is_some()
                        || this.interaction.seek_preview().is_some())
                {
                    this.cancel_interaction(window, cx);
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if this.playlist_action.is_some() {
                    if key == "escape" {
                        this.dismiss_playlist_action(window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.playlist_editor.is_some() {
                    if key == "escape" {
                        this.cancel_playlist_name(window, cx);
                        cx.stop_propagation();
                    } else if !this.input_focused(window, cx) {
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.duplicate_review.open {
                    if key == "escape" {
                        this.close_duplicate_review(window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.about_open {
                    if key == "escape" {
                        this.about_open = false;
                        this.focus_layout(window, cx);
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.settings_open {
                    // Dialog input, dropdown navigation and cancellation belong to its entity.
                    return;
                }
                if this.layouts_open {
                    if key == "escape" {
                        if this.layout_action.is_some() {
                            this.layout_action = None;
                        } else {
                            this.layouts_open = false;
                        }
                        this.focus_layout(window, cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.library_root_menu.is_some() {
                    if key == "escape" {
                        this.library_root_menu = None;
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.app_menu_open || this.panel_menu.is_some() || this.catalog_open {
                    if key == "escape" {
                        this.app_menu_open = false;
                        if this.panel_menu.is_some() {
                            this.panel_menu = None;
                        } else {
                            this.catalog_open = false;
                        }
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                if this.input_focused(window, cx) {
                    if key == "escape" {
                        this.focus_layout(window, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if this.active_list_focus() == Some(ListFocus::PlaylistTracks)
                    && !modifiers.alt
                    && !modifiers.function
                {
                    let multi = modifiers.control || modifiers.platform;
                    if key == "a" && multi && !modifiers.shift {
                        this.select_all_playlist_entries(cx);
                        cx.stop_propagation();
                        return;
                    }
                    if matches!(key, "up" | "down") && modifiers.shift {
                        this.navigate_playlist(key == "down", true, multi, cx);
                        cx.stop_propagation();
                        return;
                    }
                }
                let unmodified = !modifiers.control
                    && !modifiers.platform
                    && !modifiers.alt
                    && !modifiers.shift
                    && !modifiers.function;
                if !unmodified {
                    return;
                }
                match key {
                    "escape" => {
                        this.focus_layout(window, cx);
                        this.request_clear_selection(cx);
                    }
                    "q" => {
                        this.request_close_window(window, cx);
                    }
                    "/" => this.activate_library(window, cx, true),
                    "t" => this.activate_library(window, cx, false),
                    "up" | "down" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.browser.focus_list(focus);
                            this.navigate_list(focus, key == "down", cx);
                        }
                    }
                    "pageup" | "pagedown" => {
                        if this
                            .scroll_views
                            .page(key == "pagedown", window.mouse_position())
                        {
                            cx.notify();
                        }
                    }
                    "left" | "right" | "space" | " "
                        if this.active_list_focus() == Some(ListFocus::Library) =>
                    {
                        this.library_tree_key(key, cx);
                    }
                    "enter" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.play_focused_selection(focus, cx);
                        }
                    }
                    "d" | "delete" => {
                        if let Some(focus) = this.active_list_focus() {
                            this.remove_focused_queue(focus, cx);
                        }
                    }
                    _ => {
                        if let Some(command) = playback_key_command(key, &this.state.playback) {
                            this.send(command, cx);
                        }
                    }
                }
                cx.stop_propagation();
            }))
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, _| {
                let modal = this.manual_confirmation_open()
                    || this.settings_open
                    || this.about_open
                    || this.duplicate_review.open
                    || this.playlist_editor.is_some()
                    || this.playlist_action.is_some()
                    || this.app_menu_open
                    || this.panel_menu.is_some()
                    || this.catalog_open;
                this.scroll_views.click(event.position, modal);
            }))
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
            .h(footer_height)
            .flex_shrink_0()
            .px(gpui::px(UI_INSET))
            .pt_1()
            .pb(gpui::px(UI_INSET))
            .text_xs()
            .text_color(rgb(MUTED));
        if let Some(error) = error {
            footer = footer.child(
                row()
                    .flex_initial()
                    .min_w_0()
                    .px_2()
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(TEXT))
                    .child(copyable_message("copy-error", error, cx).flex_initial())
                    .child(components::quiet_icon_button(icon_button(
                        "dismiss-error",
                        "",
                        self.tr("shell-dismiss-message"),
                        cx,
                        |this, _, cx| {
                            this.error = None;
                            this.send(Command::DismissError, cx);
                        },
                    ))),
            );
        } else if self.state.system.scanning {
            footer = footer.child(
                copyable_message(
                    "copy-scan-message",
                    self.state.system.scan_message.clone(),
                    cx,
                )
                .text_color(rgb(TEXT)),
            );
        }
        footer = footer
            .child(div().flex_1())
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
                .ml_4()
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
            let about = menu_item(
                "app-about",
                self.tr("shell-about"),
                cx,
                |this, window, cx| {
                    this.app_menu_open = false;
                    this.about_open = true;
                    this.about_focus.focus(window, cx);
                    cx.notify();
                },
            );
            app = app.child(
                column()
                    .id("app-menu-popup")
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
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
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
                                    ACCENT
                                } else {
                                    MUTED
                                }))
                                .child(if !self.state.system.config.still_mode {
                                    self.tr("shell-on")
                                } else {
                                    self.tr("shell-off")
                                }),
                        ),
                    )
                    .child(about)
                    .child(components::menu_separator())
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
            app = app.child(self.docking_overlay());
        }

        if self.catalog_open && !self.layouts.current().locked {
            let mut choices = row().gap_1();
            let mut visualizations = row().gap_1();
            for spec in PANELS {
                let kind = spec.kind;
                let choice = button(kind, self.tr(spec.title_id), cx, move |this, _, cx| {
                    if this.layouts.current().locked {
                        return;
                    }
                    let cancellation = this.interaction.cancel();
                    this.apply_interaction_cancellation(cancellation, cx);
                    this.layouts
                        .current_mut()
                        .add(kind, this.interaction.target_group());
                    this.layout_changed(cx);
                    this.catalog_open = false;
                    this.persist_layout(cx);
                });
                if matches!(
                    kind,
                    "spectrum" | "waveform" | "spectrogram" | "audio_visualizer" | "album_artwork"
                ) {
                    visualizations = visualizations.child(choice);
                } else {
                    choices = choices.child(choice);
                }
            }
            app = app.child(
                column()
                    .id("panel-catalog")
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.catalog_open = false;
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .absolute()
                    .bottom(footer_height)
                    .right(gpui::px(UI_INSET))
                    .max_w_full()
                    .p(gpui::px(UI_INSET))
                    .gap_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.tr("shell-add-panel")),
                    )
                    .child(components::menu_separator())
                    .child(choices)
                    .child(visualizations),
            );
        }

        if self.panel_menu.is_none() {
            self.library_remove_pending = None;
        }
        if let Some(state) = self.panel_menu {
            let panel_kind = self
                .layouts
                .current()
                .panels()
                .into_iter()
                .find(|panel| panel.id == state.panel_id)
                .map(|panel| panel.kind.as_str());
            let is_queue = panel_kind == Some("queue");
            let is_library = panel_kind == Some("library");
            let is_waveform = panel_kind == Some("waveform");
            let is_audio_visualizer = panel_kind == Some("audio_visualizer");
            let is_album_artwork = panel_kind == Some("album_artwork");
            let has_selected_queue = self.browser.queue.ready() && self.browser.queue.len() > 0;
            let mut menu = context_menu_container("panel-context-menu")
                .max_w((window.viewport_size().width - px(UI_INSET * 2.)).max(px(0.)))
                .max_h((window.viewport_size().height - px(UI_INSET * 2.)).max(px(0.)))
                .overflow_y_scroll()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.panel_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }));
            match state.page {
                PanelMenuPage::Main => {
                    if is_library {
                        let can_edit = !self.library_range_pending()
                            && !self.browser.library.selected().is_empty();
                        menu = menu
                            .child(components::disabled_control(
                                menu_item(
                                    "library-enqueue",
                                    self.tr("gl-enqueue-selected"),
                                    cx,
                                    |this, _, cx| {
                                        if this.library_range_pending()
                                            || this.browser.library.selected().is_empty()
                                        {
                                            return;
                                        }
                                        this.panel_menu = None;
                                        this.send(
                                            Command::Enqueue {
                                                track_ids: this.panel_selected_tracks(),
                                            },
                                            cx,
                                        );
                                    },
                                ),
                                !can_edit,
                            ))
                            .child(components::disabled_control(
                                components::danger_control(menu_item(
                                    "library-remove",
                                    self.tr("gl-remove-library-selected"),
                                    cx,
                                    move |this, _, cx| {
                                        if this.library_range_pending()
                                            || this.browser.library.selected().is_empty()
                                        {
                                            return;
                                        }
                                        this.library_remove_pending =
                                            Some(this.panel_selected_tracks());
                                        this.panel_menu = Some(PanelMenu {
                                            page: PanelMenuPage::ConfirmRemoveLibrary,
                                            ..state
                                        });
                                        cx.notify();
                                    },
                                )),
                                !can_edit,
                            ))
                            .child(components::menu_separator());
                        menu = menu.child(menu_item(
                            "library-columns-menu",
                            self.tr("shell-track-information-menu"),
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::LibraryColumns,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                    }
                    if is_waveform && !self.state.system.config.still_mode {
                        menu = menu.child(menu_item(
                            "load-full-waveform",
                            self.tr("shell-load-waveform"),
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                this.load_full_waveform(cx);
                                cx.notify();
                            },
                        ));
                        menu = menu.child(menu_item(
                            "waveform-click-seek",
                            if self.state.system.config.waveform_seek_enabled {
                                self.tr("shell-waveform-seek-on")
                            } else {
                                self.tr("shell-waveform-seek")
                            },
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                if let Err(error) = this.handle.update_gui_config(
                                    crate::core::GuiConfigChange::ToggleWaveformSeek,
                                ) {
                                    this.error = Some(format!("{error:#}"));
                                }
                                cx.notify();
                            },
                        ));
                    }
                    if is_album_artwork {
                        menu = menu
                            .child(menu_item(
                                "album-artwork-style-menu",
                                self.i18n.format(
                                    "shell-style-menu",
                                    &[(
                                        "style",
                                        settings::album_artwork_style_label(
                                            &self.i18n,
                                            self.state.system.config.album_artwork_style,
                                        )
                                        .into(),
                                    )],
                                ),
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = Some(PanelMenu {
                                        page: PanelMenuPage::AlbumArtworkStyle,
                                        ..state
                                    });
                                    cx.notify();
                                },
                            ))
                            .child(
                                menu_item(
                                    "album-artwork-rotation",
                                    self.tr("shell-artwork-rotation"),
                                    cx,
                                    |this, _, cx| {
                                        this.panel_menu = None;
                                        if let Err(error) = this.handle.update_gui_config(
                                            crate::core::GuiConfigChange::ToggleAlbumArtworkRotation,
                                        ) {
                                            this.error = Some(format!("{error:#}"));
                                        }
                                        cx.notify();
                                    },
                                )
                                .child(
                                    div().ml_auto().child(self.tr(
                                        if self.state.system.config.album_artwork_mode
                                            == crate::config::AlbumArtworkMode::Rotating
                                        {
                                            "shell-on"
                                        } else {
                                            "shell-off"
                                        },
                                    )),
                                ),
                            )
                            .child(components::menu_separator());
                    }
                    if is_audio_visualizer
                        && let Some(options) = self
                            .layouts
                            .current()
                            .audio_visualizer_options(state.panel_id)
                    {
                        menu = menu
                            .child(menu_item(
                                "audio-visualizer-style-menu",
                                self.i18n.format(
                                    "shell-style-menu",
                                    &[(
                                        "style",
                                        settings::audio_visualizer_style_label(
                                            &self.i18n,
                                            options.style,
                                        )
                                        .into(),
                                    )],
                                ),
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = Some(PanelMenu {
                                        page: PanelMenuPage::AudioVisualizerStyle,
                                        ..state
                                    });
                                    cx.notify();
                                },
                            ))
                            .child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)));
                        menu = menu
                            .child(
                                menu_item(
                                    "audio-visualizer-grid",
                                    self.tr("shell-reference-lines"),
                                    cx,
                                    move |this, _, cx| {
                                        this.panel_menu = None;
                                        if let Some(mut options) = this
                                            .layouts
                                            .current()
                                            .audio_visualizer_options(state.panel_id)
                                        {
                                            options.reference_lines = !options.reference_lines;
                                            this.layouts
                                                .current_mut()
                                                .set_audio_visualizer_options(
                                                    state.panel_id,
                                                    options,
                                                );
                                            this.persist_layout(cx);
                                        }
                                    },
                                )
                                .child(
                                    div()
                                        .ml_auto()
                                        .text_color(rgb(if options.reference_lines {
                                            ACCENT
                                        } else {
                                            MUTED
                                        }))
                                        .child(if options.reference_lines {
                                            self.tr("shell-on")
                                        } else {
                                            self.tr("shell-off")
                                        }),
                                ),
                            )
                            .child(
                                menu_item(
                                    "audio-visualizer-labels",
                                    self.tr("shell-labels"),
                                    cx,
                                    move |this, _, cx| {
                                        this.panel_menu = None;
                                        if let Some(mut options) = this
                                            .layouts
                                            .current()
                                            .audio_visualizer_options(state.panel_id)
                                        {
                                            options.labels = !options.labels;
                                            this.layouts
                                                .current_mut()
                                                .set_audio_visualizer_options(
                                                    state.panel_id,
                                                    options,
                                                );
                                            this.persist_layout(cx);
                                        }
                                    },
                                )
                                .child(
                                    div()
                                        .ml_auto()
                                        .text_color(rgb(if options.labels {
                                            ACCENT
                                        } else {
                                            MUTED
                                        }))
                                        .child(if options.labels {
                                            self.tr("shell-on")
                                        } else {
                                            self.tr("shell-off")
                                        }),
                                ),
                            )
                            .child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)))
                            .child(menu_item(
                                "audio-visualizer-settings",
                                self.tr("shell-visualizer-settings"),
                                cx,
                                move |this, window, cx| {
                                    this.panel_menu = None;
                                    if let Some(options) = this
                                        .layouts
                                        .current()
                                        .audio_visualizer_options(state.panel_id)
                                    {
                                        this.settings.update(cx, |settings, cx| {
                                            settings.open_visualizer(
                                                state.panel_id,
                                                options,
                                                window,
                                                cx,
                                            )
                                        });
                                        this.settings_open = true;
                                        cx.notify();
                                    }
                                },
                            ));
                    }
                    if is_queue {
                        menu = menu
                            .child(components::disabled_control(
                                menu_item(
                                    "queue-add-playlist",
                                    self.tr("shell-add-playlist-menu"),
                                    cx,
                                    move |this, _, cx| {
                                        this.refresh(cx);
                                        if !this.browser.queue.ready()
                                            || this.browser.queue.len() == 0
                                        {
                                            return;
                                        }
                                        this.panel_menu = Some(PanelMenu {
                                            page: PanelMenuPage::Playlists,
                                            ..state
                                        });
                                        cx.notify();
                                    },
                                ),
                                !has_selected_queue,
                            ))
                            .child(components::disabled_control(
                                menu_item(
                                    "queue-toggle-favorite",
                                    self.tr("shell-toggle-favorite"),
                                    cx,
                                    |this, _, cx| {
                                        this.refresh(cx);
                                        let mut track_ids: Vec<_> = this
                                            .browser
                                            .queue
                                            .ordered()
                                            .into_iter()
                                            .map(|row| row.track_id)
                                            .collect();
                                        track_ids.sort_unstable();
                                        track_ids.dedup();
                                        if track_ids.is_empty() {
                                            return;
                                        }
                                        this.panel_menu = None;
                                        this.send(Command::ToggleFavorite { track_ids }, cx);
                                        cx.notify();
                                    },
                                ),
                                !has_selected_queue,
                            ))
                            .child(menu_item(
                                "randomize-queue",
                                self.tr("shell-randomize-queue"),
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::RandomizeQueue, cx);
                                    cx.notify();
                                },
                            ))
                            .child(menu_item(
                                "deduplicate-queue",
                                self.tr("shell-remove-duplicates"),
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::DeduplicateQueue, cx);
                                    cx.notify();
                                },
                            ))
                            .child(menu_item(
                                "queue-save-playlist",
                                self.tr("shell-save-playlist"),
                                cx,
                                |this, window, cx| this.save_queue_as_playlist(window, cx),
                            ))
                            .child(menu_item(
                                "clear-queue",
                                self.tr("shell-clear-queue"),
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = Some(PanelMenu {
                                        page: PanelMenuPage::ConfirmClearQueue,
                                        ..state
                                    });
                                    cx.notify();
                                },
                            ));
                    }
                    menu = menu
                        .when(
                            is_library || is_queue || is_waveform || is_audio_visualizer,
                            |view| view.child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER))),
                        )
                        .when(self.layouts.current().locked, |view| {
                            view.child(
                                div()
                                    .px(gpui::px(UI_INSET))
                                    .py_1()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child(self.tr("shell-layout-locked")),
                            )
                        })
                        .child(menu_item(
                            "toggle-tab-bar",
                            if state.tab_bar_visible {
                                self.tr("shell-hide-tab-bar")
                            } else {
                                self.tr("shell-show-tab-bar")
                            },
                            cx,
                            move |this, _, cx| {
                                this.layouts
                                    .current_mut()
                                    .set_tab_bar_visible(state.panel_id, !state.tab_bar_visible);
                                this.panel_menu = None;
                                this.persist_layout(cx);
                            },
                        ));
                }
                PanelMenuPage::LibraryColumns => {
                    menu = menu
                        .child(components::menu_title(self.tr("shell-track-information")))
                        .child(components::menu_separator());
                    for column in crate::config::LibraryColumn::ALL {
                        let enabled = self.state.system.config.library_columns.enabled(column);
                        menu = menu.child(
                            menu_item(
                                column.key(),
                                self.tr(match column {
                                    crate::config::LibraryColumn::Artist => "gl-artist",
                                    crate::config::LibraryColumn::Album => "gl-album",
                                    crate::config::LibraryColumn::Duration => "gl-duration",
                                    crate::config::LibraryColumn::Codec => "gl-codec",
                                    crate::config::LibraryColumn::Bitrate => "gl-bitrate",
                                }),
                                cx,
                                move |this, _, cx| {
                                    if let Err(error) = this.handle.update_gui_config(
                                        crate::core::GuiConfigChange::ToggleLibraryColumn(column),
                                    ) {
                                        this.error = Some(format!("{error:#}"));
                                    }
                                    cx.notify();
                                },
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .text_color(rgb(if enabled { ACCENT } else { MUTED }))
                                    .child(if enabled { "✓" } else { "" }),
                            ),
                        );
                    }
                    menu = menu
                        .child(components::menu_separator())
                        .child(components::menu_back(
                            "library-columns-back",
                            self.tr("shell-back"),
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::Main,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                }
                PanelMenuPage::AlbumArtworkStyle => {
                    menu = menu
                        .child(components::menu_title(self.tr("shell-artwork-style")))
                        .child(components::menu_separator());
                    for style in settings::ALBUM_ARTWORK_STYLES {
                        let selected = self.state.system.config.album_artwork_style == style;
                        let label = self.tr(settings::album_artwork_style_id(style));
                        menu = menu.child(
                            menu_item(
                                ("album-artwork-style", style as usize),
                                label,
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = None;
                                    if let Err(error) = this.handle.update_gui_config(
                                        crate::core::GuiConfigChange::AlbumArtworkStyle(style),
                                    ) {
                                        this.error = Some(format!("{error:#}"));
                                    }
                                    this.refresh(cx);
                                    cx.notify();
                                },
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .text_color(rgb(if selected { ACCENT } else { MUTED }))
                                    .child(if selected { "●" } else { "○" }),
                            ),
                        );
                    }
                    menu = menu
                        .child(components::menu_separator())
                        .child(components::menu_back(
                            "album-artwork-style-back",
                            self.tr("shell-back"),
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::Main,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                }
                PanelMenuPage::AudioVisualizerStyle => {
                    menu = menu
                        .child(components::menu_title(self.tr("shell-visualizer-style")))
                        .child(components::menu_separator());
                    if let Some(options) = self
                        .layouts
                        .current()
                        .audio_visualizer_options(state.panel_id)
                    {
                        for style in [
                            AudioVisualizerStyle::Bars,
                            AudioVisualizerStyle::Rings,
                            AudioVisualizerStyle::BarsRings,
                            AudioVisualizerStyle::StereoDensity,
                            AudioVisualizerStyle::SpatialSpectrum,
                            AudioVisualizerStyle::FractalBeams,
                            AudioVisualizerStyle::FractalFlow,
                        ] {
                            let selected = options.style == style;
                            menu = menu.child(
                                menu_item(
                                    ("audio-visualizer-style", style as usize),
                                    self.tr(settings::audio_visualizer_style_id(style)),
                                    cx,
                                    move |this, _, cx| {
                                        this.panel_menu = None;
                                        if let Some(mut options) = this
                                            .layouts
                                            .current()
                                            .audio_visualizer_options(state.panel_id)
                                        {
                                            options.style = style;
                                            this.layouts
                                                .current_mut()
                                                .set_audio_visualizer_options(
                                                    state.panel_id,
                                                    options,
                                                );
                                            this.persist_layout(cx);
                                        }
                                    },
                                )
                                .child(
                                    div()
                                        .ml_auto()
                                        .text_color(rgb(if selected { ACCENT } else { MUTED }))
                                        .child(if selected { "●" } else { "○" }),
                                ),
                            );
                        }
                    }
                    menu = menu
                        .child(components::menu_separator())
                        .child(components::menu_back(
                            "audio-visualizer-style-back",
                            self.tr("shell-back"),
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::Main,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                }
                PanelMenuPage::ConfirmRemoveLibrary => {
                    let count = self.library_remove_pending.as_ref().map_or(0, Vec::len);
                    menu = menu
                        .child(
                            div().px(gpui::px(UI_INSET)).py_2().text_sm().child(
                                self.i18n.format(
                                    "gl-confirm-remove-library",
                                    &[("count", count.into())],
                                ),
                            ),
                        )
                        .child(menu_item(
                            "cancel-remove-library",
                            self.tr("gl-cancel"),
                            cx,
                            |this, _, cx| {
                                this.library_remove_pending = None;
                                this.panel_menu = None;
                                cx.notify();
                            },
                        ))
                        .child(components::danger_control(menu_item(
                            "confirm-remove-library",
                            self.tr("gl-remove-library-selected"),
                            cx,
                            |this, _, cx| {
                                this.panel_menu = None;
                                if let Some(track_ids) = this.library_remove_pending.take()
                                    && !track_ids.is_empty()
                                {
                                    this.send(Command::RemoveTracks { track_ids }, cx);
                                }
                                cx.notify();
                            },
                        )));
                }
                PanelMenuPage::ConfirmClearQueue => {
                    menu = menu
                        .child(div().px(gpui::px(UI_INSET)).py_2().text_sm().child(
                            self.i18n.format(
                                "shell-clear-queue-confirm",
                                &[("count", self.state.queue.total.into())],
                            ),
                        ))
                        .child(
                            panels::caption(self.tr("shell-clear-stops-playback"))
                                .px(gpui::px(UI_INSET))
                                .pb_2(),
                        )
                        .child(
                            column()
                                .child(menu_item(
                                    "cancel-clear-queue",
                                    self.tr("shell-cancel"),
                                    cx,
                                    |this, _, cx| {
                                        this.panel_menu = None;
                                        cx.notify();
                                    },
                                ))
                                .child(components::danger_control(menu_item(
                                    "confirm-clear-queue",
                                    self.tr("shell-clear-queue"),
                                    cx,
                                    |this, _, cx| {
                                        this.panel_menu = None;
                                        this.browser.queue.clear();

                                        this.send(Command::ClearQueue, cx);
                                        cx.notify();
                                    },
                                ))),
                        );
                }
                PanelMenuPage::Playlists => {
                    menu = menu
                        .child(components::menu_title(self.tr("shell-add-playlist")))
                        .child(components::menu_separator());
                    menu = menu.child(
                        menu_item(
                            "queue-create-playlist",
                            self.tr("shell-new-playlist"),
                            cx,
                            |this, window, cx| {
                                let mut seen = HashSet::new();
                                let track_ids: Vec<_> = this
                                    .browser
                                    .queue
                                    .ordered()
                                    .iter()
                                    .map(|row| row.track_id)
                                    .filter(|id| seen.insert(*id))
                                    .collect();
                                if track_ids.is_empty() {
                                    return;
                                }
                                this.begin_playlist_name(
                                    super::playlist_editor::PlaylistNameAction::Create {
                                        track_ids,
                                    },
                                    String::new(),
                                    window,
                                    cx,
                                );
                            },
                        )
                        .w_full(),
                    );
                    if has_selected_queue {
                        if !self.browser.playlists.summaries().rows.is_empty() {
                            let row_height = window.rem_size() * 2.0;
                            let height = (row_height
                                * self.browser.playlists.summaries().rows.len() as f32)
                                .min(window.rem_size() * components::POPOVER_MAX_REMS)
                                .min(window.viewport_size().height * 0.5);
                            menu = menu.child(
                                uniform_list(
                                    "queue-playlist-targets",
                                    self.browser.playlists.summaries().rows.len(),
                                    cx.processor(
                                        move |this, range: std::ops::Range<usize>, _, cx| {
                                            range
                                                .map(|index| {
                                                    let Some(playlist) = this
                                                        .browser
                                                        .playlists
                                                        .summaries()
                                                        .rows
                                                        .get(index)
                                                    else {
                                                        return this
                                                            .paged_loading_row(
                                                                ViewRequestKind::Summaries,
                                                                index,
                                                                cx,
                                                            )
                                                            .h(row_height);
                                                    };
                                                    let playlist_id = playlist.id;
                                                    menu_item(
                                                        ("queue-playlist", playlist_id as u64),
                                                        div()
                                                            .min_w_0()
                                                            .truncate()
                                                            .child(playlist.name.clone()),
                                                        cx,
                                                        move |this, _, cx| {
                                                            let track_ids = this
                                                                .browser
                                                                .queue
                                                                .ordered()
                                                                .iter()
                                                                .map(|row| row.track_id)
                                                                .collect();
                                                            this.panel_menu = None;
                                                            this.send(
                                                                Command::AddPlaylist {
                                                                    playlist_id,
                                                                    track_ids,
                                                                },
                                                                cx,
                                                            );
                                                            cx.notify();
                                                        },
                                                    )
                                                    .h(row_height)
                                                    .w_full()
                                                    .overflow_hidden()
                                                })
                                                .collect()
                                        },
                                    ),
                                )
                                .with_decoration(paged::viewport_demand(cx.processor(
                                    |this, range, _, cx| {
                                        this.ensure_playlist_range(range, cx);
                                    },
                                )))
                                .h(height)
                                .flex_shrink_0()
                                .w_full(),
                            );
                        }
                    } else {
                        menu =
                            menu.child(panels::caption(self.tr("shell-queue-entry-unavailable")));
                    }
                    menu = menu
                        .child(components::menu_separator())
                        .child(components::menu_back(
                            "queue-playlist-back",
                            self.tr("shell-back"),
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::Main,
                                    ..state
                                });
                                cx.notify();
                            },
                        ));
                }
            }
            app = app.child(
                anchored()
                    .position(state.position)
                    .snap_to_window()
                    .child(menu),
            );
        }

        if let Some((root, position)) = self.library_root_menu.clone() {
            let menu = context_menu_container("library-root-menu")
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.library_root_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(menu_item(
                    "remove-library-root",
                    self.tr("shell-remove-library"),
                    cx,
                    move |this, _, cx| {
                        this.library_root_menu = None;
                        if let Err(error) = this.handle.update_gui_config(
                            crate::core::GuiConfigChange::RemoveLibraryRoot(root.clone()),
                        ) {
                            this.error = Some(format!("{error:#}"));
                        }
                        this.refresh(cx);
                        cx.notify();
                    },
                ));
            app = app.child(anchored().position(position).snap_to_window().child(menu));
        }

        if self.playlist_action.is_some() {
            app = app.child(self.playlist_action_overlay(window, cx));
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
                div()
                    .id("about-modal")
                    .track_focus(&self.about_focus)
                    .occlude()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x000000).alpha(0.58))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|_, _, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                    }))
                    .child(
                        row()
                            .id("about-card")
                            .relative()
                            .max_w(relative(0.9))
                            .items_center()
                            .gap_6()
                            .p_12()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
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
                                            .child(caption(self.tr("shell-tagline")))
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
                                                                .child(caption(label)),
                                                        )
                                                        .child(caption(value))
                                                }),
                                            ),
                                    ),
                            )
                            .child(
                                components::quiet_icon_button(icon_button(
                                    "about-close",
                                    "",
                                    self.tr("shell-close-about"),
                                    cx,
                                    |this, window, cx| {
                                        this.about_open = false;
                                        this.focus_layout(window, cx);
                                        cx.notify();
                                    },
                                ))
                                .absolute()
                                .top(px(8.))
                                .right(px(8.)),
                            ),
                    ),
            );
        }
        if let Some(confirmation) = self.render_manual_confirmation(cx) {
            app = app.child(confirmation);
        }
        self.scroll_views.finish_frame();
        app
    }
}
