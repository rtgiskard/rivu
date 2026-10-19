//! Panel catalog and context-menu presentation, options, and layout actions.

use super::super::*;
use crate::config::AudioVisualizerStyle;
use rivu_ui::{
    Popover, button, caption, checked_menu_item, column, menu_item, row, surface::MenuIndicator,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum PanelMenuPage {
    Main,
    AudioVisualizerStyle,
    AlbumArtworkStyle,
    ConfirmClearQueue,
    LibraryColumns,
}

#[derive(Clone, Copy)]
pub(in crate::gui) struct PanelMenu {
    pub(in crate::gui) panel_id: u64,
    pub(in crate::gui) tab_bar_visible: bool,
    pub(in crate::gui) position: Point<Pixels>,
    pub(in crate::gui) page: PanelMenuPage,
}

impl GuiApp {
    pub(in crate::gui) fn open_keyboard_panel_menu(
        &mut self,
        panel_id: u64,
        tab_bar_visible: bool,
        anchor: Measured,
        cx: &mut Context<Self>,
    ) {
        let position = self
            .workspace
            .interaction
            .measurements()
            .get(&anchor)
            .map(|bounds| bounds.bottom_left());
        let Some(position) = position else {
            return;
        };
        self.panel_menu = Some(PanelMenu {
            panel_id,
            tab_bar_visible,
            position,
            page: PanelMenuPage::Main,
        });
        cx.notify();
    }

    pub(in crate::gui) fn render_panel_catalog(
        &self,
        theme: &'static rivu_ui::theme::Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let mut choices = row().gap_1().flex_wrap();
        let mut visualizations = row().gap_1().flex_wrap();
        for spec in PANELS {
            let kind = spec.kind;
            let title = self.tr(spec.title_id);
            let choice = button(kind, title.clone(), cx, move |this, _, cx| {
                if this.workspace.layouts.current().locked {
                    return;
                }
                let cancellation = this.workspace.interaction.cancel();
                this.apply_interaction_cancellation(cancellation, cx);
                this.workspace
                    .layouts
                    .current_mut()
                    .add(kind, this.workspace.interaction.target_group());
                this.layout_changed(cx);
                this.shell.dismiss(shell::Surface::PanelCatalog);
                this.persist_layout(cx);
            })
            .flex_shrink_0()
            .max_w_full()
            .truncate()
            .tooltip(rivu_ui::text_tooltip(title));
            if matches!(
                kind,
                "spectrum" | "waveform" | "spectrogram" | "audio_visualizer" | "album_artwork"
            ) {
                visualizations = visualizations.child(choice);
            } else {
                choices = choices.child(choice);
            }
        }
        Popover::surface(theme, "panel-catalog")
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .map(|surface| self.menu_surface(surface, cx))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.shell.dismiss(shell::Surface::PanelCatalog);
                cx.stop_propagation();
                cx.notify();
            }))
            .max_w_full()
            .p_2()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.muted))
                    .child(self.tr("shell-add-panel")),
            )
            .child(rivu_ui::menu_separator(theme))
            .child(choices)
            .child(visualizations)
    }

    pub(in crate::gui) fn render_panel_menu(
        &self,
        state: PanelMenu,
        theme: &'static rivu_ui::theme::Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel_kind = self
            .workspace
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
        let mut menu = Popover::menu(theme, "panel-context-menu", window)
            .map(|surface| self.menu_surface(surface, cx))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.panel_menu = None;
                cx.stop_propagation();
                cx.notify();
            }));
        match state.page {
            PanelMenuPage::Main => {
                if is_library {
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
                            if let Err(error) = this
                                .handle
                                .update_gui_config(crate::core::GuiConfigChange::ToggleWaveformSeek)
                            {
                                this.error = Some(format!("{error:#}"));
                            }
                            cx.notify();
                        },
                    ));
                }
                if let Some(options) = self
                    .workspace
                    .layouts
                    .current()
                    .album_artwork_options(state.panel_id)
                    .filter(|_| is_album_artwork)
                {
                    menu = menu
                        .child(menu_item(
                            "album-artwork-style-menu",
                            self.i18n.format(
                                "shell-style-menu",
                                &[(
                                    "style",
                                    album_artwork::album_artwork_style_label(
                                        &self.i18n,
                                        options.style,
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
                        .child(checked_menu_item(
                            "album-artwork-rotation",
                            self.tr("shell-artwork-rotation"),
                            options.mode == crate::config::AlbumArtworkMode::Rotating,
                            MenuIndicator::Check,
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = None;
                                if let Some(mut options) = this
                                    .workspace
                                    .layouts
                                    .current()
                                    .album_artwork_options(state.panel_id)
                                {
                                    options.mode = match options.mode {
                                        crate::config::AlbumArtworkMode::Static => {
                                            crate::config::AlbumArtworkMode::Rotating
                                        }
                                        crate::config::AlbumArtworkMode::Rotating => {
                                            crate::config::AlbumArtworkMode::Static
                                        }
                                    };
                                    this.workspace
                                        .layouts
                                        .current_mut()
                                        .set_album_artwork_options(state.panel_id, options);
                                    this.persist_layout(cx);
                                }
                                cx.notify();
                            },
                        ))
                        .child(menu_item(
                            "album-artwork-settings",
                            self.tr("shell-artwork-settings"),
                            cx,
                            move |this, window, cx| {
                                this.panel_menu = None;
                                if let Some(options) = this
                                    .workspace
                                    .layouts
                                    .current()
                                    .album_artwork_options(state.panel_id)
                                {
                                    this.settings.update(cx, |settings, cx| {
                                        settings.open_artwork(state.panel_id, options, window, cx)
                                    });
                                    this.shell.open(shell::Surface::Settings);
                                    cx.notify();
                                }
                            },
                        ))
                        .child(rivu_ui::menu_separator(theme));
                }
                if is_audio_visualizer
                    && let Some(options) = self
                        .workspace
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
                                    visualization::audio_visualizer_style_label(
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
                        .child(div().h(px(1.)).mx_2().my_1().bg(rgb(theme.border)));
                    menu = menu
                        .child(checked_menu_item(
                            "audio-visualizer-grid",
                            self.tr("shell-reference-lines"),
                            options.reference_lines,
                            MenuIndicator::Check,
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = None;
                                if let Some(mut options) = this
                                    .workspace
                                    .layouts
                                    .current()
                                    .audio_visualizer_options(state.panel_id)
                                {
                                    options.reference_lines = !options.reference_lines;
                                    this.workspace
                                        .layouts
                                        .current_mut()
                                        .set_audio_visualizer_options(state.panel_id, options);
                                    this.persist_layout(cx);
                                }
                            },
                        ))
                        .child(checked_menu_item(
                            "audio-visualizer-labels",
                            self.tr("shell-labels"),
                            options.labels,
                            MenuIndicator::Check,
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = None;
                                if let Some(mut options) = this
                                    .workspace
                                    .layouts
                                    .current()
                                    .audio_visualizer_options(state.panel_id)
                                {
                                    options.labels = !options.labels;
                                    this.workspace
                                        .layouts
                                        .current_mut()
                                        .set_audio_visualizer_options(state.panel_id, options);
                                    this.persist_layout(cx);
                                }
                            },
                        ))
                        .child(div().h(px(1.)).mx_2().my_1().bg(rgb(theme.border)))
                        .child(menu_item(
                            "audio-visualizer-settings",
                            self.tr("shell-visualizer-settings"),
                            cx,
                            move |this, window, cx| {
                                this.panel_menu = None;
                                if let Some(options) = this
                                    .workspace
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
                                    this.shell.open(shell::Surface::Settings);
                                    cx.notify();
                                }
                            },
                        ));
                }
                if is_queue {
                    menu = menu
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
                        .child(rivu_ui::danger_menu_item(
                            theme,
                            menu_item(
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
                            ),
                        ));
                }
                menu = menu
                    .when(
                        is_library || is_queue || is_waveform || is_audio_visualizer,
                        |view| view.child(div().h(px(1.)).mx_2().my_1().bg(rgb(theme.border))),
                    )
                    .child(menu_item(
                        "toggle-tab-bar",
                        if state.tab_bar_visible {
                            self.tr("shell-hide-tab-bar")
                        } else {
                            self.tr("shell-show-tab-bar")
                        },
                        cx,
                        move |this, _, cx| {
                            this.workspace
                                .layouts
                                .current_mut()
                                .set_tab_bar_visible(state.panel_id, !state.tab_bar_visible);
                            this.panel_menu = None;
                            this.persist_layout(cx);
                        },
                    ));
            }
            PanelMenuPage::LibraryColumns => {
                menu = menu
                    .child(rivu_ui::menu_title(
                        theme,
                        self.tr("shell-track-information"),
                    ))
                    .child(rivu_ui::menu_separator(theme));
                for column in crate::config::LibraryColumn::ALL {
                    let enabled = self.state.system.config.library_columns.enabled(column);
                    menu = menu.child(checked_menu_item(
                        column.key(),
                        self.tr(match column {
                            crate::config::LibraryColumn::Artist => "gl-artist",
                            crate::config::LibraryColumn::Album => "gl-album",
                            crate::config::LibraryColumn::Duration => "gl-duration",
                            crate::config::LibraryColumn::Codec => "gl-codec",
                            crate::config::LibraryColumn::Bitrate => "gl-bitrate",
                        }),
                        enabled,
                        MenuIndicator::Check,
                        cx,
                        move |this, _, cx| {
                            if let Err(error) = this.handle.update_gui_config(
                                crate::core::GuiConfigChange::ToggleLibraryColumn(column),
                            ) {
                                this.error = Some(format!("{error:#}"));
                            }
                            cx.notify();
                        },
                    ));
                }
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::menu_back(
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
                    .child(rivu_ui::menu_title(theme, self.tr("shell-artwork-style")))
                    .child(rivu_ui::menu_separator(theme));
                for style in album_artwork::ALBUM_ARTWORK_STYLES {
                    let selected = self
                        .workspace
                        .layouts
                        .current()
                        .album_artwork_options(state.panel_id)
                        .is_some_and(|options| options.style == style);
                    let label = self.tr(album_artwork::album_artwork_style_id(style));
                    menu = menu.child(checked_menu_item(
                        ("album-artwork-style", style as usize),
                        label,
                        selected,
                        MenuIndicator::Radio,
                        cx,
                        move |this, _, cx| {
                            this.panel_menu = None;
                            if let Some(mut options) = this
                                .workspace
                                .layouts
                                .current()
                                .album_artwork_options(state.panel_id)
                            {
                                options.style = style;
                                if this
                                    .workspace
                                    .layouts
                                    .current_mut()
                                    .set_album_artwork_options(state.panel_id, options)
                                {
                                    this.persist_layout(cx);
                                }
                            }
                            cx.notify();
                        },
                    ));
                }
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::menu_back(
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
                    .child(rivu_ui::menu_title(
                        theme,
                        self.tr("shell-visualizer-style"),
                    ))
                    .child(rivu_ui::menu_separator(theme));
                if let Some(options) = self
                    .workspace
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
                        menu = menu.child(checked_menu_item(
                            ("audio-visualizer-style", style as usize),
                            self.tr(visualization::audio_visualizer_style_id(style)),
                            selected,
                            MenuIndicator::Radio,
                            cx,
                            move |this, _, cx| {
                                this.panel_menu = None;
                                if let Some(mut options) = this
                                    .workspace
                                    .layouts
                                    .current()
                                    .audio_visualizer_options(state.panel_id)
                                {
                                    options.style = style;
                                    this.workspace
                                        .layouts
                                        .current_mut()
                                        .set_audio_visualizer_options(state.panel_id, options);
                                    this.persist_layout(cx);
                                }
                            },
                        ));
                    }
                }
                menu = menu
                    .child(rivu_ui::menu_separator(theme))
                    .child(rivu_ui::menu_back(
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
            PanelMenuPage::ConfirmClearQueue => {
                menu = menu
                    .child(div().px_2().py_2().text_sm().child(self.i18n.format(
                        "shell-clear-queue-confirm",
                        &[("count", self.state.queue.total.into())],
                    )))
                    .child(
                        caption(theme, self.tr("shell-clear-stops-playback"))
                            .px_2()
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
                            .child(rivu_ui::danger_menu_item(
                                theme,
                                menu_item(
                                    "confirm-clear-queue",
                                    self.tr("shell-clear-queue"),
                                    cx,
                                    |this, _, cx| {
                                        this.panel_menu = None;
                                        this.queue
                                            .update(cx, |queue, cx| queue.clear_selection(cx));

                                        this.send(Command::ClearQueue, cx);
                                        cx.notify();
                                    },
                                ),
                            )),
                    );
            }
        }
        anchored()
            .position(state.position)
            .snap_to_window()
            .child(menu)
            .into_any_element()
    }
}
