//! Settings page composition; session transitions and control text remain with the owner.
use super::{
    Field, Measured, SettingChoice, Settings, SettingsEvent, SettingsPage, VisualPage,
    dropdown_button, ffmpeg_hint, model::Target, settings_card, settings_group,
    spectrum_style_label, visual_switch, visualization_palette_label,
};
use crate::{
    config::{
        AlbumArtworkDirection, AlbumArtworkMode, AnalysisWindow, AudioVisualizerStyle, LogLevel,
        SpectrumStyle, VisualizationPalette, VisualizationSignal,
    },
    gui::{
        album_artwork::ALBUM_ARTWORK_STYLES,
        visualization::{AUDIO_VISUALIZER_STYLES, audio_visualizer_style_label},
    },
    theme::Theme,
};
use gpui::{Context, Div, SharedString, div, prelude::*, rems, rgb};
use rivu_ui::{
    DropdownItem, FormField, StatusKind, button, caption, column, copyable_message,
    disabled_control, row, status_message, text_tooltip, theme::palette, two_column,
};

impl Settings {
    pub(super) fn artwork_cache_label(
        &self,
        colors: &'static rivu_ui::theme::Palette,
        label: SharedString,
        help_key: &'static str,
    ) -> Div {
        row().gap_2().child(caption(colors, label)).child(
            div()
                .id(help_key)
                .flex_shrink_0()
                .child(
                    caption(colors, "")
                        .opacity(0.6)
                        .text_color(rgb(colors.text)),
                )
                .tooltip(text_tooltip(self.tr(help_key))),
        )
    }

    fn artwork_cache_settings(&self, cx: &mut Context<Self>) -> Div {
        let colors = palette(cx);
        let busy = self.artwork_cache_busy.is_some();
        let mut content = column().gap_2().child(
            column()
                .w_full()
                .flex_shrink_0()
                .gap_1()
                .child(self.artwork_cache_label(
                    colors,
                    self.tr("settings-expire-files-older-than-days"),
                    "settings-artwork-disk-help",
                ))
                .child(
                    row()
                        .items_center()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            div()
                                .id("settings-artwork-cache-age")
                                .w(rems(7.))
                                .flex_shrink_0()
                                .child(self.artwork_cache_age.clone()),
                        )
                        .child(
                            row()
                                .gap_2()
                                .flex_shrink_0()
                                .child(
                                    rivu_ui::IconButton::new(
                                        "settings-artwork-cache-calculate",
                                        "󰃬",
                                        self.tr("settings-calculate-usage"),
                                    )
                                    .enabled(!(busy))
                                    .build(
                                        rivu_ui::IconButtonStyle::Surface,
                                        rivu_ui::IconButtonDensity::Regular,
                                        cx,
                                        |this, _, cx| this.maintain_artwork_cache(false, cx),
                                    ),
                                )
                                .child(
                                    rivu_ui::IconButton::new(
                                        "settings-artwork-cache-clean",
                                        "󰆴",
                                        self.tr("settings-clean-expired"),
                                    )
                                    .enabled(!(busy))
                                    .build(
                                        rivu_ui::IconButtonStyle::Surface,
                                        rivu_ui::IconButtonDensity::Regular,
                                        cx,
                                        |this, _, cx| this.maintain_artwork_cache(true, cx),
                                    ),
                                ),
                        ),
                ),
        );
        content = column()
            .gap_2()
            .child(self.single(Field::ArtworkFileLimit, cx))
            .child(self.single(Field::ArtworkDecodeBudget, cx))
            .child(content);
        if let Some((days, clean)) = self.artwork_cache_busy {
            content = content.child(status_message(
                colors,
                self.i18n.format(
                    if clean {
                        "settings-cache-cleaning"
                    } else {
                        "settings-cache-calculating"
                    },
                    &[("days", days.to_string().into())],
                ),
                StatusKind::Info,
            ));
        }
        if let Some((days, clean, report)) = &self.artwork_cache_report {
            content = content
                .child(status_message(
                    colors,
                    self.i18n.format(
                        if *clean {
                            "settings-cache-cleaned"
                        } else {
                            "settings-cache-calculated"
                        },
                        &[("days", days.to_string().into())],
                    ),
                    if report.failed_files == 0 {
                        StatusKind::Success
                    } else {
                        StatusKind::Error
                    },
                ))
                .child(caption(
                    colors,
                    self.i18n.format(
                        "settings-cache-total",
                        &[
                            ("files", report.files.to_string().into()),
                            ("bytes", report.bytes.to_string().into()),
                        ],
                    ),
                ))
                .child(caption(
                    colors,
                    self.i18n.format(
                        "settings-cache-expired",
                        &[
                            ("files", report.expired_files.to_string().into()),
                            ("bytes", report.expired_bytes.to_string().into()),
                        ],
                    ),
                ))
                .child(caption(
                    colors,
                    self.i18n.format(
                        "settings-cache-protected",
                        &[
                            ("files", report.protected_files.to_string().into()),
                            ("bytes", report.protected_bytes.to_string().into()),
                        ],
                    ),
                ))
                .child(caption(
                    colors,
                    self.i18n.format(
                        "settings-cache-removed",
                        &[
                            ("files", report.removed_files.to_string().into()),
                            ("bytes", report.removed_bytes.to_string().into()),
                        ],
                    ),
                ))
                .child(status_message(
                    colors,
                    self.i18n.format(
                        "settings-cache-failures",
                        &[("count", report.failed_files.to_string().into())],
                    ),
                    if report.failed_files == 0 {
                        StatusKind::Info
                    } else {
                        StatusKind::Error
                    },
                ));
            if let Some(error) = &report.first_error {
                content = content.child(
                    copyable_message(
                        "copy-artwork-cache-file-error",
                        self.tr("shell-copy-message"),
                        error.clone(),
                        cx,
                    )
                    .text_color(rgb(colors.error)),
                );
            }
            if report.failed_files > 0 {
                content =
                    content.child(caption(colors, self.tr("settings-cache-permissions-help")));
            }
        }
        if let Some(error) = &self.artwork_cache_error {
            content = content.child(
                copyable_message(
                    "copy-artwork-cache-error",
                    self.tr("shell-copy-message"),
                    error.clone(),
                    cx,
                )
                .text_color(rgb(colors.error)),
            );
        }
        settings_card(colors, self.tr("settings-artwork-cache"), content)
    }

    fn library_roots(&self, cx: &mut Context<Self>) -> Div {
        let colors = palette(cx);
        let mut paths = column()
            .id("settings-library-roots-list")
            .max_h_40()
            .pl_3()
            .overflow_y_scroll()
            .gap_1();
        for (index, path) in self.model.draft.library_roots.iter().enumerate() {
            let stats = self.root_stats_label(path);
            let stats_hint: SharedString = self
                .root_stats_error()
                .map(|error| SharedString::from(error.to_owned()))
                .unwrap_or_else(|| self.tr("settings-library-root-stats-hint"));
            let path: SharedString = path.to_string_lossy().into_owned().into();
            paths = paths.child(
                row()
                    .id(("settings-root-row", index))
                    .group("settings-root-row")
                    .gap_1()
                    .child(
                        div()
                            .id(("settings-library-root-path", index))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_middle()
                            .child(path.clone())
                            .tooltip(text_tooltip(path)),
                    )
                    .child(
                        div()
                            .id(("settings-library-root-stats", index))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_color(rgb(colors.muted))
                            .child(stats)
                            .tooltip(text_tooltip(stats_hint)),
                    )
                    .child(rivu_ui::hover_reveal(
                        ("settings-root-remove-reveal", index),
                        "settings-root-row",
                        rivu_ui::IconButton::new(
                            ("settings-library-root-remove", index),
                            "",
                            self.tr("settings-remove-library-folder"),
                        )
                        .build(
                            rivu_ui::IconButtonStyle::Quiet,
                            rivu_ui::IconButtonDensity::Regular,
                            cx,
                            move |this, _, cx| {
                                this.model.draft.library_roots.remove(index);
                                this.sync_library_stats(cx);
                                cx.notify();
                            },
                        ),
                    )),
            );
        }
        if self.model.draft.library_roots.is_empty() {
            paths = paths.child(caption(colors, self.tr("settings-library-folders-empty")));
        }
        column()
            .gap_2()
            .child(
                row()
                    .gap_2()
                    .child(caption(colors, self.tr("settings-library-folders")))
                    .child(
                        div()
                            .id("settings-library-folders-help")
                            .child(
                                caption(colors, "")
                                    .opacity(0.6)
                                    .hover(|style| style.opacity(1.)),
                            )
                            .tooltip(text_tooltip(self.tr("settings-library-folders-add-hint"))),
                    ),
            )
            .child(paths)
    }

    pub(super) fn library_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let library = settings_card(
            palette(cx),
            self.tr("settings-library"),
            column()
                .gap_2()
                .child(self.library_roots(cx))
                .child(self.pair(Field::ScanMaxDepth, Field::ScanMinDuration, cx)),
        );
        column()
            .gap_3()
            .child(library)
            .child(settings_card(
                palette(cx),
                self.tr("settings-relocation"),
                column()
                    .gap_2()
                    .child(caption(palette(cx), self.tr("settings-relocation-help")))
                    .child(row().child(button(
                        "settings-same-files",
                        self.tr("settings-review-candidates"),
                        cx,
                        |this, _, cx| {
                            this.dropdown.close();
                            cx.emit(SettingsEvent::ReviewDuplicates);
                        },
                    ))),
            ))
            .child(self.artwork_cache_settings(cx))
    }

    pub(super) fn playback_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let chosen_device: SharedString = self
            .model
            .draft
            .output_device
            .clone()
            .unwrap_or_else(|| self.tr("settings-system-default").to_string())
            .into();
        let device = column()
            .flex_shrink_0()
            .gap_1()
            .child(caption(palette(cx), self.tr("settings-output-device")))
            .child(
                dropdown_button(
                    "output-device-selector",
                    self.tr("settings-output-device"),
                    chosen_device.clone(),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::Device,
                    cx,
                    |this, _, _| {
                        let open = this.dropdown.is_open();
                        if open {
                            this.dropdown.close();
                        } else {
                            let selected = this
                                .model
                                .draft
                                .output_device
                                .as_ref()
                                .and_then(|name| {
                                    this.devices.iter().position(|device| device == name)
                                })
                                .map_or(0, |index| index + 1);
                            let items = std::iter::once(DropdownItem::new(
                                SettingChoice::Device(0),
                                this.i18n.text("settings-system-default").to_owned(),
                            ))
                            .chain(this.devices.iter().enumerate().map(|(index, device)| {
                                DropdownItem::new(SettingChoice::Device(index + 1), device.clone())
                            }))
                            .collect();
                            this.open_setting_dropdown(items, selected);
                        }
                    },
                )
                .relative()
                .tooltip({
                    let chosen_device = chosen_device.clone();
                    rivu_ui::text_tooltip(chosen_device)
                })
                .child(self.measurement(Measured::Device, cx)),
            );
        let ffmpeg = self.model.draft.ffmpeg_enabled;
        let remix = self.model.draft.pipewire_auto_mix;
        let playback = settings_card(
            palette(cx),
            self.tr("settings-playback"),
            column()
                .gap_2()
                .child(self.pair(Field::Volume, Field::PlayCountThreshold, cx))
                .child(
                    visual_switch(
                        &self.i18n,
                        "settings-confirm-queue-replace",
                        "󰁨",
                        self.tr("settings-confirm-queue-replace"),
                        self.model.draft.confirm_queue_replace,
                        cx,
                        |draft| draft.confirm_queue_replace = !draft.confirm_queue_replace,
                    )
                    .self_start(),
                ),
        );
        let audio = settings_card(
            palette(cx),
            self.tr("settings-audio"),
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(device.flex_1().min_w(rems(260. / 16.)))
                        .child(
                            self.field(Field::MediaReadBuffer, cx)
                                .flex_1()
                                .min_w(rems(240. / 16.)),
                        ),
                )
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .child(rivu_ui::toggle_button(
                            "settings-ffmpeg",
                            "\u{f384}",
                            self.i18n.format(
                                "settings-ffmpeg-status",
                                &[
                                    (
                                        "state",
                                        self.i18n
                                            .text(if ffmpeg {
                                                "settings-on"
                                            } else {
                                                "settings-off"
                                            })
                                            .into(),
                                    ),
                                    (
                                        "detail",
                                        ffmpeg_hint(&self.i18n, &self.ffmpeg_status)
                                            .to_owned()
                                            .into(),
                                    ),
                                ],
                            ),
                            ffmpeg,
                            cx,
                            |this, _| {
                                this.model.draft.ffmpeg_enabled = !this.model.draft.ffmpeg_enabled;
                            },
                        ))
                        .child(visual_switch(
                            &self.i18n,
                            "settings-pipewire-auto-mix",
                            "󱀞",
                            self.tr("settings-pipewire-channel-remix"),
                            remix,
                            cx,
                            |draft| draft.pipewire_auto_mix = !draft.pipewire_auto_mix,
                        )),
                ),
        );
        column().gap_3().child(playback).child(audio)
    }

    pub(super) fn basic_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let colors = palette(cx);
        let draft = &self.model.draft;
        let language = dropdown_button(
            "settings-language",
            self.tr("settings-language"),
            match draft.ui_language.as_str() {
                "en-US" => SharedString::from("English"),
                "zh-CN" => SharedString::from("简体中文"),
                _ => self.tr("settings-language-auto"),
            },
            self.dropdown.is_open() && self.dropdown_anchor == Measured::Language,
            cx,
            |this, _, _| {
                let values = ["auto", "en-US", "zh-CN"];
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.ui_language)
                    .unwrap_or(0);
                let labels = [
                    this.i18n.text("settings-language-auto"),
                    "English",
                    "简体中文",
                ];
                let items = labels
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        DropdownItem::new(SettingChoice::Language(index), label.to_owned())
                    })
                    .collect();
                this.open_setting_dropdown(items, selected);
            },
        )
        .tooltip(text_tooltip(self.tr("settings-language-restart")))
        .relative()
        .child(self.measurement(Measured::Language, cx));
        let theme = dropdown_button(
            "settings-theme",
            self.tr("settings-theme"),
            draft.ui_theme.label(),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::Theme,
            cx,
            |this, _, _| {
                let selected = Theme::ALL
                    .iter()
                    .position(|value| *value == this.model.draft.ui_theme)
                    .unwrap_or(0);
                let items = Theme::ALL
                    .into_iter()
                    .map(|value| DropdownItem::new(SettingChoice::Theme(value), value.label()))
                    .collect();
                this.open_setting_dropdown(items, selected);
            },
        )
        .tooltip(text_tooltip(self.tr("settings-theme-hint")))
        .relative()
        .child(self.measurement(Measured::Theme, cx));
        settings_card(
            colors,
            self.tr("settings-appearance-layout"),
            column()
                .gap_2()
                .child(two_column(
                    FormField::new(colors, self.tr("settings-theme"), theme).into_div(),
                    div(),
                ))
                .child(two_column(
                    FormField::new(colors, self.tr("settings-language"), language).into_div(),
                    div(),
                ))
                .child(two_column(
                    self.font_control(cx).min_w(rems(180. / 16.)),
                    self.field(Field::FontSize, cx).min_w(rems(180. / 16.)),
                ))
                .child(two_column(
                    self.field(Field::Scale, cx).min_w(rems(180. / 16.)),
                    self.layout_settings(cx),
                )),
        )
    }

    pub(super) fn layout_settings(&mut self, cx: &mut Context<Self>) -> Div {
        column().child(
            Field::LayoutSnapGrid.form(
                palette(cx),
                self,
                row()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.inputs[&Field::LayoutSnapGrid].clone()),
                    )
                    .child(visual_switch(
                        &self.i18n,
                        "settings-layout-snapping",
                        "󰐃",
                        self.tr("settings-layout-snapping"),
                        self.model.draft.layout_snap_enabled,
                        cx,
                        |draft| draft.layout_snap_enabled = !draft.layout_snap_enabled,
                    )),
            ),
        )
    }

    pub(super) fn desktop_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let mpris = self.model.draft.mpris_enabled;
        let tray = self.model.draft.tray_enabled;
        column().child(
            row()
                .flex_wrap()
                .gap_2()
                .child(rivu_ui::toggle_button(
                    "settings-mpris",
                    "󰐹",
                    self.i18n.format(
                        "settings-mpris-status",
                        &[
                            (
                                "state",
                                self.i18n
                                    .text(if mpris { "settings-on" } else { "settings-off" })
                                    .into(),
                            ),
                            ("detail", self.mpris_status.to_owned().into()),
                        ],
                    ),
                    mpris,
                    cx,
                    |this, _| {
                        this.model.draft.mpris_enabled = !this.model.draft.mpris_enabled;
                    },
                ))
                .child(visual_switch(
                    &self.i18n,
                    "settings-tray",
                    "",
                    self.tr("settings-system-tray"),
                    tray,
                    cx,
                    |draft| draft.tray_enabled = !draft.tray_enabled,
                )),
        )
    }

    pub(super) fn logging_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let draft = &self.model.draft;
        let log_level_dropdown = dropdown_button(
            "settings-log-level",
            self.tr("settings-log-level"),
            SettingChoice::LogLevel(draft.log_level).label(&self.i18n),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsLogLevel,
            cx,
            |this, _, _| {
                let values = [
                    LogLevel::Debug,
                    LogLevel::Info,
                    LogLevel::Warning,
                    LogLevel::Error,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.log_level)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            DropdownItem::new(
                                SettingChoice::LogLevel(value),
                                SettingChoice::LogLevel(value).label(&this.i18n),
                            )
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::SettingsLogLevel, cx));
        settings_card(
            palette(cx),
            self.tr("settings-logging"),
            column()
                .gap_2()
                .child(
                    row()
                        .items_start()
                        .flex_wrap()
                        .gap_3()
                        .child(
                            column()
                                .flex_1()
                                .min_w(rems(180. / 16.))
                                .gap_1()
                                .child(caption(palette(cx), self.tr("settings-log-level")))
                                .child(log_level_dropdown),
                        )
                        .child(
                            self.field(Field::LogRetentionWeeks, cx)
                                .flex_1()
                                .min_w(rems(180. / 16.)),
                        ),
                )
                .child(row().child(visual_switch(
                    &self.i18n,
                    "settings-log-file",
                    "",
                    self.tr("settings-write-logs-to-disk"),
                    draft.log_to_file,
                    cx,
                    |draft| draft.log_to_file = !draft.log_to_file,
                )))
                .id("settings-logging-scope-help")
                .tooltip(text_tooltip(self.tr("settings-logging-scope"))),
        )
    }

    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let mut sidebar = column()
            .gap_1()
            .w(rems(11.))
            .flex_shrink_0()
            .p_2()
            .border_r_1()
            .border_color(rgb(palette(cx).border));
        sidebar = sidebar.child(self.tab_button(
            "nav-library",
            self.tr("settings-library"),
            self.page == SettingsPage::Library,
            cx,
            |settings| settings.page = SettingsPage::Library,
        ));
        sidebar = sidebar.child(self.tab_button(
            "nav-playback-audio",
            self.tr("settings-playback-audio"),
            self.page == SettingsPage::Playback,
            cx,
            |settings| settings.page = SettingsPage::Playback,
        ));
        sidebar = sidebar.child(self.tab_button(
            "nav-interface",
            self.tr("settings-interface"),
            self.page == SettingsPage::Interface,
            cx,
            |settings| settings.page = SettingsPage::Interface,
        ));
        sidebar = sidebar.child(self.tab_button(
            "nav-visualizations",
            self.tr("settings-visualizations"),
            self.page == SettingsPage::Visualizations,
            cx,
            |settings| settings.page = SettingsPage::Visualizations,
        ));
        if self.page == SettingsPage::Visualizations {
            let mut children = column().pl_3().gap_1();
            for page in VisualPage::ALL {
                children = children.child(self.tab_button(
                    page.scroll_id(),
                    self.tr(page.label_id()),
                    self.visual_page == page,
                    cx,
                    move |settings| settings.visual_page = page,
                ));
            }
            sidebar = sidebar.child(children);
        }
        sidebar
    }

    pub(super) fn artwork_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let colors = palette(cx);
        let settings = &*self;
        let draft = &settings.model.draft;
        let style = dropdown_button(
            "album-artwork-style",
            self.tr("settings-style"),
            SettingChoice::AlbumArtworkStyle(draft.album_artwork_style).label(&self.i18n),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::AlbumArtworkStyle,
            cx,
            |this, _, _| {
                let values = ALBUM_ARTWORK_STYLES;
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.album_artwork_style)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkStyle(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkStyle, cx));
        let mode = dropdown_button(
            "album-artwork-mode",
            self.tr("settings-mode"),
            SettingChoice::AlbumArtworkMode(draft.album_artwork_mode).label(&self.i18n),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::AlbumArtworkMode,
            cx,
            |this, _, _| {
                let values = [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating];
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.album_artwork_mode)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkMode(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkMode, cx));
        let direction = dropdown_button(
            "album-artwork-direction",
            self.tr("settings-direction"),
            SettingChoice::AlbumArtworkDirection(draft.album_artwork_direction).label(&self.i18n),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::AlbumArtworkDirection,
            cx,
            |this, _, _| {
                let values = [
                    AlbumArtworkDirection::Clockwise,
                    AlbumArtworkDirection::Counterclockwise,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.album_artwork_direction)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AlbumArtworkDirection(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AlbumArtworkDirection, cx));
        let rotating = draft.album_artwork_mode == AlbumArtworkMode::Rotating;
        column()
            .gap_3()
            .child(settings_group(
                colors,
                settings.tr(if matches!(settings.model.target(), Target::Artwork(_)) {
                    "settings-this-instance"
                } else {
                    "settings-new-instance-defaults"
                }),
                row().child(
                    FormField::new(colors, settings.tr("settings-style"), style)
                        .into_div()
                        .w(gpui::relative(0.5))
                        .min_w(rems(180. / 16.))
                        .pr_2(),
                ),
            ))
            .child(settings_group(
                colors,
                settings.tr("settings-animation"),
                column()
                    .gap_2()
                    .child(two_column(
                        FormField::new(colors, settings.tr("settings-mode"), mode)
                            .into_div()
                            .min_w(rems(180. / 16.)),
                        div().min_w(rems(180. / 16.)).child(
                            FormField::new(
                                colors,
                                settings.tr("settings-direction"),
                                disabled_control(colors, direction, !rotating),
                            )
                            .into_div()
                            .id("settings-artwork-direction-field"),
                        ),
                    ))
                    .child(two_column(
                        settings.field_state(Field::AlbumArtworkPeriod, cx),
                        column().when(
                            !matches!(settings.model.target(), Target::Artwork(_)),
                            |column| {
                                column
                                    .min_w(rems(180. / 16.))
                                    .gap_2()
                                    .child(settings.field_state(Field::AlbumArtworkFps, cx))
                            },
                        ),
                    )),
            ))
    }

    fn visualizer_display_settings(&self, cx: &mut Context<Self>) -> Div {
        let draft = &self.model.draft;
        let instance = matches!(self.model.target(), Target::Visualizer(_));
        let style = dropdown_button(
            "audio-visualizer-style",
            self.tr("settings-style"),
            audio_visualizer_style_label(&self.i18n, draft.audio_visualizer_style),
            self.dropdown.is_open() && self.dropdown_anchor == Measured::AudioVisualizerStyle,
            cx,
            |this, _, _| {
                let values = AUDIO_VISUALIZER_STYLES;
                let selected = values
                    .iter()
                    .position(|value| *value == this.model.draft.audio_visualizer_style)
                    .unwrap_or(0);
                this.open_setting_dropdown(
                    values
                        .into_iter()
                        .map(|value| {
                            let choice = SettingChoice::AudioVisualizerStyle(value);
                            DropdownItem::new(choice, choice.label(&this.i18n))
                        })
                        .collect(),
                    selected,
                );
            },
        )
        .relative()
        .child(self.measurement(Measured::AudioVisualizerStyle, cx));
        settings_card(
            palette(cx),
            self.tr(if instance {
                "settings-this-instance"
            } else {
                "settings-new-instance-defaults"
            }),
            column()
                .gap_2()
                .child(FormField::new(palette(cx), self.tr("settings-style"), style).into_div())
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .when(
                            !matches!(
                                draft.audio_visualizer_style,
                                AudioVisualizerStyle::Waveform | AudioVisualizerStyle::Spectrogram
                            ),
                            |controls| {
                                controls.child(visual_switch(
                                    &self.i18n,
                                    "audio-visualizer-grid",
                                    "",
                                    self.tr("settings-reference-lines"),
                                    draft.audio_visualizer_grid,
                                    cx,
                                    |draft| {
                                        draft.audio_visualizer_grid = !draft.audio_visualizer_grid
                                    },
                                ))
                            },
                        )
                        .child(visual_switch(
                            &self.i18n,
                            "audio-visualizer-labels",
                            "",
                            self.tr("settings-labels"),
                            draft.audio_visualizer_labels,
                            cx,
                            |draft| draft.audio_visualizer_labels = !draft.audio_visualizer_labels,
                        )),
                )
                .child(div().text_sm().child(self.tr(if instance {
                    "settings-visualizer-instance-help"
                } else {
                    "settings-visualizer-defaults-help"
                }))),
        )
    }

    fn visualizer_response_settings(&self, cx: &mut Context<Self>) -> Div {
        settings_card(
            palette(cx),
            self.tr("settings-response-motion"),
            column()
                .gap_2()
                .child(self.single(Field::AudioVisualizerDb, cx))
                .child(self.pair(
                    Field::AudioVisualizerSensitivity,
                    Field::AudioVisualizerBloomIntensity,
                    cx,
                )),
        )
    }

    fn visualizer_color_settings(&self, cx: &mut Context<Self>) -> Div {
        settings_card(
            palette(cx),
            self.tr("settings-colors"),
            self.pair(
                Field::AudioVisualizerPrimaryColor,
                Field::AudioVisualizerSecondaryColor,
                cx,
            ),
        )
    }

    pub(super) fn visual_settings(&mut self, cx: &mut Context<Settings>) -> Div {
        let settings = &*self;
        let draft = &settings.model.draft;
        if matches!(settings.model.target(), Target::Visualizer(_)) {
            return column()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .child(settings.tr(settings.visual_page.label_id())),
                )
                .child(settings.visualizer_display_settings(cx));
        }
        let scope = div()
            .text_sm()
            .child(settings.tr("settings-visualizer-global-help"));
        let content = match settings.visual_page {
            VisualPage::Common => {
                let palette_control = dropdown_button(
                    "visual-palette",
                    self.tr("settings-color-scheme"),
                    visualization_palette_label(&self.i18n, draft.visual_palette),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsStyle,
                    cx,
                    |this, _, _| {
                        let values = [
                            VisualizationPalette::TokyoNight,
                            VisualizationPalette::Deadbeef,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.model.draft.visual_palette)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Palette(value),
                                        visualization_palette_label(&this.i18n, value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle, cx));
                let fft = dropdown_button(
                    "analysis-fft",
                    self.tr("settings-fft-size-samples"),
                    draft.analysis_fft_size.to_string(),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsFft,
                    cx,
                    |this, _, _| {
                        let values = [512, 1024, 2048, 4096, 8192, 16384, 32768];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.model.draft.analysis_fft_size)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(SettingChoice::Fft(value), value.to_string())
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsFft, cx));
                let window = dropdown_button(
                    "analysis-window",
                    self.tr("settings-fft-window"),
                    SettingChoice::Window(draft.analysis_window).label(&self.i18n),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsWindow,
                    cx,
                    |this, _, _| {
                        let values = [
                            AnalysisWindow::Hann,
                            AnalysisWindow::BlackmanHarris,
                            AnalysisWindow::None,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.model.draft.analysis_window)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Window(value),
                                        SettingChoice::Window(value).label(&this.i18n),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsWindow, cx));
                let signal = dropdown_button(
                    "visualization-signal",
                    self.tr("settings-visualization-signal"),
                    SettingChoice::Signal(draft.visualization_signal).label(&self.i18n),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsSignal,
                    cx,
                    |this, _, _| {
                        let values = [VisualizationSignal::Before, VisualizationSignal::After];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.model.draft.visualization_signal)
                            .unwrap_or(1);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::Signal(value),
                                        SettingChoice::Signal(value).label(&this.i18n),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsSignal, cx));
                column()
                    .gap_3()
                    .child(settings.visualizer_display_settings(cx))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-appearance"),
                        column()
                            .gap_2()
                            .child(two_column(
                                settings
                                    .field(Field::Background, cx)
                                    .min_w(rems(180. / 16.)),
                                FormField::new(
                                    palette(cx),
                                    settings.tr("settings-color-scheme"),
                                    palette_control,
                                )
                                .into_div()
                                .min_w(rems(180. / 16.)),
                            ))
                            .id("settings-appearance-help")
                            .tooltip(text_tooltip(settings.tr("settings-appearance-help"))),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-display-range"),
                        settings.single(Field::VisualizationDb, cx),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-animation"),
                        settings.single(Field::VisualizationFps, cx),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-frequency-analysis"),
                        column()
                            .gap_2()
                            .child(two_column(
                                FormField::new(
                                    palette(cx),
                                    settings.tr("settings-fft-size-samples"),
                                    fft,
                                )
                                .into_div()
                                .min_w(rems(180. / 16.)),
                                FormField::new(
                                    palette(cx),
                                    settings.tr("settings-fft-window"),
                                    window,
                                )
                                .into_div()
                                .min_w(rems(180. / 16.)),
                            ))
                            .child(settings.single(Field::Fps, cx))
                            .id("settings-analysis-help")
                            .tooltip(text_tooltip(settings.tr("settings-analysis-help"))),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-processing"),
                        column().gap_2().child(
                            row().child(
                                FormField::new(
                                    palette(cx),
                                    settings.tr("settings-visualization-signal"),
                                    signal,
                                )
                                .into_div()
                                .w(gpui::relative(0.5))
                                .min_w(rems(180. / 16.))
                                .pr_2()
                                .id("settings-visualization-signal-help")
                                .tooltip(text_tooltip(
                                    settings.tr("settings-visualization-signal-hint"),
                                )),
                            ),
                        ),
                    ))
            }
            VisualPage::Spectrum => {
                let style = dropdown_button(
                    "spectrum-style",
                    self.tr("settings-style"),
                    spectrum_style_label(&self.i18n, draft.spectrum_style),
                    self.dropdown.is_open() && self.dropdown_anchor == Measured::SettingsStyle,
                    cx,
                    |this, _, _| {
                        let values = [
                            SpectrumStyle::Bars,
                            SpectrumStyle::Outline,
                            SpectrumStyle::Led,
                            SpectrumStyle::Line,
                            SpectrumStyle::Solid,
                        ];
                        let selected = values
                            .iter()
                            .position(|value| *value == this.model.draft.spectrum_style)
                            .unwrap_or(0);
                        this.open_setting_dropdown(
                            values
                                .into_iter()
                                .map(|value| {
                                    DropdownItem::new(
                                        SettingChoice::SpectrumStyle(value),
                                        spectrum_style_label(&this.i18n, value),
                                    )
                                })
                                .collect(),
                            selected,
                        );
                    },
                )
                .relative()
                .child(self.measurement(Measured::SettingsStyle, cx));
                column()
                    .gap_3()
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-appearance"),
                        column()
                            .gap_2()
                            .child(two_column(
                                FormField::new(palette(cx), settings.tr("settings-style"), style)
                                    .into_div()
                                    .min_w(rems(180. / 16.)),
                                settings.field_state(Field::SpectrumBars, cx),
                            ))
                            .child(two_column(
                                settings.field_state(Field::SpectrumBarWidth, cx),
                                settings.field_state(Field::SpectrumGap, cx),
                            ))
                            .child(settings.single(Field::SpectrumDb, cx))
                            .child(row().flex_wrap().child(visual_switch(
                                &self.i18n,
                                "spectrum-interpolate",
                                "",
                                settings.tr("settings-frequency-interpolation"),
                                draft.spectrum_interpolate,
                                cx,
                                |draft| draft.spectrum_interpolate = !draft.spectrum_interpolate,
                            ))),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-bar-motion"),
                        column()
                            .gap_2()
                            .child(settings.pair(
                                Field::SpectrumBarHold,
                                Field::SpectrumBarGravity,
                                cx,
                            ))
                            .child(settings.single(Field::SpectrumSmoothing, cx)),
                    ))
                    .child(settings_card(
                        palette(cx),
                        settings.tr("settings-peak-markers"),
                        column()
                            .gap_2()
                            .child(row().child(visual_switch(
                                &self.i18n,
                                "spectrum-peaks",
                                "",
                                settings.tr("settings-peak-markers"),
                                draft.spectrum_peaks,
                                cx,
                                |draft| draft.spectrum_peaks = !draft.spectrum_peaks,
                            )))
                            .child(two_column(
                                settings.field_state(Field::SpectrumHold, cx),
                                settings.field_state(Field::SpectrumGravity, cx),
                            )),
                    ))
            }
            VisualPage::Spectrogram => column()
                .gap_3()
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-display"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::SpectrogramDb, Field::SpectrogramHistory, cx))
                        .child(settings.pair(
                            Field::SpectrogramFlowSpeed,
                            Field::SpectrogramAccumulation,
                            cx,
                        )),
                ))
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-frequency-sampling"),
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::SpectrogramSamplingPointsScale,
                            Field::SpectrogramInterpolationPoints,
                            cx,
                        ))
                        .child(row().child(visual_switch(
                            &self.i18n,
                            "spectrogram-interpolate",
                            "",
                            settings.tr("settings-frequency-interpolation"),
                            draft.spectrogram_interpolate,
                            cx,
                            |draft| draft.spectrogram_interpolate = !draft.spectrogram_interpolate,
                        ))),
                )),
            VisualPage::BarsRings => column()
                .gap_3()
                .child(settings.visualizer_response_settings(cx))
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-bars-rings-section"),
                    column()
                        .gap_2()
                        .child(settings.pair(
                            Field::AudioVisualizerInnerDiameter,
                            Field::AudioVisualizerRotationSpeed,
                            cx,
                        ))
                        .child(settings.pair(
                            Field::AudioVisualizerBarWidth,
                            Field::AudioVisualizerBarGlowLayers,
                            cx,
                        ))
                        .child(settings.single(Field::AudioVisualizerRingOpacity, cx))
                        .child(row().child(visual_switch(
                            &self.i18n,
                            "audio-visualizer-fade-idle",
                            "",
                            settings.tr("settings-fade-when-idle"),
                            draft.audio_visualizer_fade_when_idle,
                            cx,
                            |draft| {
                                draft.audio_visualizer_fade_when_idle =
                                    !draft.audio_visualizer_fade_when_idle
                            },
                        )))
                        .id("settings-bars-rings-help")
                        .tooltip(text_tooltip(
                            settings.tr("settings-visualizer-bars-rings-help"),
                        )),
                ))
                .child(settings.visualizer_color_settings(cx)),
            VisualPage::StereoDensity => column()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .child(settings.tr("settings-stereo-density-hint")),
                )
                .child(settings.visualizer_color_settings(cx)),
            VisualPage::SpatialSpectrum => column()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .child(settings.tr("settings-spatial-spectrum-hint")),
                )
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-display-range"),
                    settings.single(Field::AudioVisualizerDb, cx),
                ))
                .child(settings.visualizer_color_settings(cx)),
            VisualPage::Fractal => column()
                .gap_3()
                .child(settings.visualizer_response_settings(cx))
                .child(settings.visualizer_color_settings(cx)),
            VisualPage::Oscilloscope => column()
                .gap_3()
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-display"),
                    settings.pair(Field::OscilloscopeTimeSpan, Field::OscilloscopeGain, cx),
                ))
                .child(settings.visualizer_color_settings(cx)),
            VisualPage::Other => settings_card(
                palette(cx),
                settings.tr("settings-processing"),
                column()
                    .gap_2()
                    .child(settings.single(Field::GpuMaterialBudget, cx))
                    .child(row().child(visual_switch(
                        &self.i18n,
                        "settings-still-mode",
                        "",
                        settings.tr("settings-still-mode"),
                        draft.still_mode,
                        cx,
                        |draft| draft.still_mode = !draft.still_mode,
                    ))),
            ),
            VisualPage::Waveform => column()
                .gap_3()
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-appearance-interaction"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::CursorColor, Field::Glow, cx))
                        .child(row().flex_wrap().gap_2().child(visual_switch(
                            &self.i18n,
                            "waveform-seek",
                            "",
                            settings.tr("settings-click-and-drag-to-seek"),
                            draft.waveform_seek_enabled,
                            cx,
                            |draft| draft.waveform_seek_enabled = !draft.waveform_seek_enabled,
                        ))),
                ))
                .child(settings_card(
                    palette(cx),
                    settings.tr("settings-amplitude-display"),
                    column()
                        .gap_2()
                        .child(settings.pair(Field::WaveformRmsGain, Field::WaveformPeakGain, cx))
                        .child(settings.single(Field::WaveformPeakGamma, cx))
                        .id("settings-amplitude-help")
                        .tooltip(text_tooltip(
                            settings.tr("settings-waveform-amplitude-help"),
                        )),
                )),
        };
        column().gap_3().child(scope).child(content)
    }
}
