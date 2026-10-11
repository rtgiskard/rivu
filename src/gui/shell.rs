//! Top-level composition and menus; panel content remains in its existing modules.

use super::*;
use crate::{config::AudioVisualizerStyle, model::playback_key_command};

#[derive(Clone, Copy)]
pub(super) enum PanelMenuPage {
    Main,
    AudioVisualizerStyle,
    ConfirmClearQueue,
    Playlists,
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
                    .child("Add a panel to build your layout")
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
                    window.remove_window();
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
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
                        this.browser.clear_selection();
                        this.clear_metadata_selection(cx);
                        cx.notify();
                    }
                    "q" => {
                        window.remove_window();
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
                        if this.browser.library.tree_active()
                            && this.active_list_focus() == Some(ListFocus::Library) =>
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
                let modal = this.settings_open
                    || this.about_open
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
                    move |_, _, window, _cx| {
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
        let mut footer = row()
            .h(rems(3.0))
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
                    .child(
                        icon_button(
                            "dismiss-error",
                            "",
                            "Dismiss message",
                            cx,
                            |this, _, cx| {
                                this.error = None;
                                this.send(Command::DismissError, cx);
                            },
                        )
                        .size(px(24.0))
                        .border_0()
                        .bg(rgb(ERROR_BG))
                        .text_color(rgb(MUTED)),
                    ),
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
                        "Layout locked\nRight-click to unlock"
                    } else {
                        "Left-click: add panel\nRight-click: lock layout"
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
                        this.persist_layout(cx);
                    }),
                ),
            )
            .child(icon_button("app-menu", "", "Menu", cx, |this, _, cx| {
                this.app_menu_open = !this.app_menu_open;
                this.catalog_open = false;
                this.panel_menu = None;
                cx.notify();
            }));
        if self.app_menu_open {
            let about = menu_item("app-about", "About", cx, |this, window, cx| {
                this.app_menu_open = false;
                this.about_open = true;
                this.about_focus.focus(window, cx);
                cx.notify();
            });
            app = app.child(
                column()
                    .id("app-menu-popup")
                    .occlude()
                    .absolute()
                    .bottom(rems(3.0))
                    .right(gpui::px(UI_INSET))
                    .w(px(220.))
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
                        "Settings",
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
                        "Layouts",
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
                        menu_item("app-still-mode", "Still mode", cx, |this, _, cx| {
                            this.app_menu_open = false;
                            if let Err(error) = this
                                .handle
                                .update_gui_config(crate::core::GuiConfigChange::ToggleStillMode)
                            {
                                this.error = Some(format!("{error:#}"));
                            }
                            cx.notify();
                        })
                        .child(
                            div()
                                .ml_auto()
                                .text_color(rgb(if self.state.system.config.still_mode {
                                    ACCENT
                                } else {
                                    MUTED
                                }))
                                .child(if self.state.system.config.still_mode {
                                    "On"
                                } else {
                                    "Off"
                                }),
                        ),
                    )
                    .child(about)
                    .child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)))
                    .child(menu_item(
                        "app-close-window",
                        "Close window",
                        cx,
                        |this, window, _| {
                            this.app_menu_open = false;
                            window.remove_window();
                        },
                    ))
                    .child(menu_item(
                        "app-quit",
                        "Quit application",
                        cx,
                        |this, _, cx| {
                            this.app_menu_open = false;
                            this.send(Command::Shutdown, cx);
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
                let choice = button(kind, spec.title, cx, move |this, _, cx| {
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
                    "spectrum" | "waveform" | "spectrogram" | "audio_visualizer"
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
                    .bottom(rems(3.0))
                    .right(gpui::px(UI_INSET))
                    .max_w_full()
                    .p(gpui::px(UI_INSET))
                    .gap_1()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Add panel"))
                    .child(choices)
                    .child(visualizations),
            );
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
            let is_waveform = panel_kind == Some("waveform");
            let is_audio_visualizer = panel_kind == Some("audio_visualizer");
            let has_selected_queue = self
                .state
                .queue
                .entries
                .iter()
                .any(|entry| self.browser.queue.contains(&entry.id));
            let mut menu = context_menu_container("panel-context-menu").on_mouse_down_out(
                cx.listener(|this, _, _, cx| {
                    this.panel_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
            match state.page {
                PanelMenuPage::Main => {
                    if is_waveform && !self.state.system.config.still_mode {
                        menu = menu.child(menu_item(
                            "load-full-waveform",
                            "Load full waveform now",
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
                                "Click and drag to seek ✓"
                            } else {
                                "Click and drag to seek"
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
                    if is_audio_visualizer {
                        if let Some(options) = self
                            .layouts
                            .current()
                            .audio_visualizer_options(state.panel_id)
                        {
                            menu = menu
                                .child(menu_item(
                                    "audio-visualizer-style-menu",
                                    format!(
                                        "Style: {}  ›",
                                        settings::audio_visualizer_style_label(options.style)
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
                                        "Reference lines",
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
                                                "On"
                                            } else {
                                                "Off"
                                            }),
                                    ),
                                )
                                .child(
                                    menu_item(
                                        "audio-visualizer-labels",
                                        "Labels",
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
                                            .child(if options.labels { "On" } else { "Off" }),
                                    ),
                                )
                                .child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)))
                                .child(menu_item(
                                    "audio-visualizer-settings",
                                    "Visualizer settings…",
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
                    }
                    if is_queue {
                        menu = menu
                            .child(menu_item(
                                "randomize-queue",
                                "Randomize queue",
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::RandomizeQueue, cx);
                                    cx.notify();
                                },
                            ))
                            .child(menu_item(
                                "deduplicate-queue",
                                "Remove duplicates",
                                cx,
                                |this, _, cx| {
                                    this.panel_menu = None;
                                    this.send(Command::DeduplicateQueue, cx);
                                    cx.notify();
                                },
                            ));
                        if has_selected_queue {
                            menu = menu.child(menu_item(
                                "queue-add-playlist",
                                "Add to playlist  ›",
                                cx,
                                move |this, _, cx| {
                                    this.panel_menu = Some(PanelMenu {
                                        page: PanelMenuPage::Playlists,
                                        ..state
                                    });
                                    cx.notify();
                                },
                            ));
                        }
                        menu = menu.child(menu_item(
                            "clear-queue",
                            "Clear queue",
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
                        .when(is_queue || is_waveform || is_audio_visualizer, |view| {
                            view.child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)))
                        })
                        .when(self.layouts.current().locked, |view| {
                            view.child(
                                div()
                                    .px(gpui::px(UI_INSET))
                                    .py_1()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("Layout is locked"),
                            )
                        })
                        .when(!self.layouts.current().locked, |view| {
                            view.child(menu_item(
                                "toggle-tab-bar",
                                if state.tab_bar_visible {
                                    "Hide tab bar"
                                } else {
                                    "Show tab bar"
                                },
                                cx,
                                move |this, _, cx| {
                                    if this.layouts.current().locked {
                                        return;
                                    }
                                    this.layouts.current_mut().set_tab_bar_visible(
                                        state.panel_id,
                                        !state.tab_bar_visible,
                                    );
                                    this.panel_menu = None;
                                    this.persist_layout(cx);
                                },
                            ))
                        });
                }
                PanelMenuPage::AudioVisualizerStyle => {
                    menu = menu
                        .child(menu_item(
                            "audio-visualizer-style-back",
                            "‹ Back",
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = Some(PanelMenu {
                                    page: PanelMenuPage::Main,
                                    ..state
                                });
                                cx.notify();
                            },
                        ))
                        .child(div().h(px(1.)).mx_2().my_1().bg(rgb(BORDER)));
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
                                    settings::audio_visualizer_style_label(style),
                                    settings::audio_visualizer_style_label(style),
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
                }
                PanelMenuPage::ConfirmClearQueue => {
                    menu = menu
                        .child(div().px(gpui::px(UI_INSET)).py_2().text_sm().child(format!(
                            "Clear all {} queued entries?",
                            self.state.queue.entries.len()
                        )))
                        .child(
                            panels::caption("This also stops playback.")
                                .px(gpui::px(UI_INSET))
                                .pb_2(),
                        )
                        .child(
                            row()
                                .child(
                                    menu_item("cancel-clear-queue", "Cancel", cx, |this, _, cx| {
                                        this.panel_menu = None;
                                        cx.notify();
                                    })
                                    .flex_1(),
                                )
                                .child(
                                    menu_item(
                                        "confirm-clear-queue",
                                        "Clear queue",
                                        cx,
                                        |this, _, cx| {
                                            this.panel_menu = None;
                                            this.browser.queue.clear();

                                            this.send(Command::ClearQueue, cx);
                                            cx.notify();
                                        },
                                    )
                                    .flex_1()
                                    .text_color(rgb(ERROR)),
                                ),
                        );
                }
                PanelMenuPage::Playlists => {
                    menu = menu.child(
                        menu_item(
                            "queue-create-playlist",
                            "Save selected queue as new playlist",
                            cx,
                            |this, _, cx| {
                                let name = this.value(Field::PlaylistName, cx).trim().to_owned();
                                if name.is_empty() {
                                    this.panel_error(
                                        "Enter a playlist name in Playlists first.",
                                        cx,
                                    );
                                    return;
                                }
                                let mut track_ids = Vec::new();
                                for entry in this.state.queue.entries.iter() {
                                    if this.browser.queue.contains(&entry.id)
                                        && !track_ids.contains(&entry.track_id)
                                    {
                                        track_ids.push(entry.track_id);
                                    }
                                }
                                if track_ids.is_empty() {
                                    return;
                                }
                                this.panel_menu = None;
                                this.send(
                                    Command::CreatePlaylistWithTracks { name, track_ids },
                                    cx,
                                );
                            },
                        )
                        .w_full(),
                    );
                    if has_selected_queue {
                        if !self.browser.playlists.summaries.rows.is_empty() {
                            let height = (self.browser.playlists.summaries.rows.len() as f32
                                * panels::TRACK_HEIGHT)
                                .min(240.)
                                .min(f32::from(window.viewport_size().height) * 0.5);
                            menu =
                                menu.child(
                                    uniform_list(
                                        "queue-playlist-targets",
                                        self.browser.playlists.summaries.rows.len(),
                                        cx.processor(
                                            move |this, range: std::ops::Range<usize>, _, cx| {
                                                range
                                                    .map(|index| {
                                                        let Some(playlist) = this
                                                            .browser
                                                            .playlists
                                                            .summaries
                                                            .rows
                                                            .get(index)
                                                        else {
                                                            return this.paged_loading_row(
                                                            ViewRequestKind::PlaylistSummaries,
                                                            index,
                                                            cx,
                                                        ).h(px(panels::TRACK_HEIGHT));
                                                        };
                                                        let playlist_id = playlist.id;
                                                        menu_item(
                                                            ("queue-playlist", playlist_id as u64),
                                                            playlist.name.clone(),
                                                            cx,
                                                            move |this, _, cx| {
                                                                let track_ids = this
                                                                    .state
                                                                    .queue
                                                                    .entries
                                                                    .iter()
                                                                    .filter(|entry| {
                                                                        this.browser
                                                                            .queue
                                                                            .contains(&entry.id)
                                                                    })
                                                                    .map(|entry| entry.track_id)
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
                                                        .h(px(panels::TRACK_HEIGHT))
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
                                    .h(px(height))
                                    .w_full(),
                                );
                        }
                    } else {
                        menu = menu.child(panels::caption(
                            "The selected queue entry is no longer available.",
                        ));
                    }
                    menu = menu.child(menu_item(
                        "queue-playlist-back",
                        "Back",
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
                                    .child(caption("A local-first music player"))
                                    .children(
                                        [
                                            (
                                                "Version",
                                                format!(
                                                    "{} (features: {})",
                                                    env!("RIVU_GIT_VERSION"),
                                                    env!("RIVU_BUILD_FEATURES"),
                                                ),
                                            ),
                                            ("Built", built),
                                            ("License", "GPL-3.0-or-later".to_owned()),
                                        ]
                                        .into_iter()
                                        .map(
                                            |(label, value)| {
                                                row()
                                                    .gap_3()
                                                    .child(
                                                        div()
                                                            .w(rems(4.))
                                                            .flex_shrink_0()
                                                            .child(caption(label)),
                                                    )
                                                    .child(caption(value))
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                icon_button(
                                    "about-close",
                                    "",
                                    "Close",
                                    cx,
                                    |this, window, cx| {
                                        this.about_open = false;
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
        self.scroll_views.finish_frame();
        app
    }
}
