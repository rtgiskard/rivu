//! Export drafts bind a source explicitly; playback changes never retarget a draft.
use super::*;
use rivu_dsp::equalizer::config::EqualizerConfig;
use rivu_format::config::processing_graph::{GraphNodeKind, GraphPort};
use rivu_protocol::model::{
    AudioExportEncoding, AudioExportFormat, AudioExportRequest, AudioExportSource,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Source {
    #[default]
    Original,
    Graph(GraphPort),
    Frozen {
        id: u64,
        target: GraphPort,
    },
}

#[derive(Default)]
pub(super) struct ExportState {
    initialized: bool,
    source: Source,
    track: Option<Track>,
    frozen_path: Option<PathBuf>,
    generation: u64,
    monitor_eq: Option<EqualizerConfig>,
    encoding: AudioExportEncoding,
    advanced: bool,
}

fn format_name(format: AudioExportFormat) -> &'static str {
    match format {
        AudioExportFormat::Wav => "wav",
        AudioExportFormat::Flac => "flac",
        AudioExportFormat::Opus => "opus",
        AudioExportFormat::Mp3 => "mp3",
    }
}

fn export_source(
    source: Source,
    track: Option<&Track>,
    revision: u64,
) -> Option<AudioExportSource> {
    Some(match source {
        Source::Original => AudioExportSource::Original {
            track_id: track?.id,
        },
        Source::Graph(target) => AudioExportSource::Graph {
            track_id: track?.id,
            target,
            revision,
        },
        Source::Frozen { id, target } => AudioExportSource::Frozen { id, target },
    })
}

impl AudioProcessing {
    pub(super) fn probe_export_formats(&mut self, cx: &mut Context<Self>) {
        if self.state.system.export_formats.is_none() {
            self.send(Command::ProbeAudioExportFormats, cx);
        }
    }

    fn export_format_available(&self, format: AudioExportFormat) -> bool {
        self.state
            .system
            .export_formats
            .as_ref()
            .is_some_and(|formats| formats.contains(&format))
    }

    pub(super) fn initialize_export(&mut self, cx: &mut Context<Self>) {
        if self.export.initialized {
            return;
        }
        self.export.initialized = true;
        self.export.track = self.input_track().cloned();
        for (key, label, value) in [
            (
                FieldKey::ExportPath,
                "processing-export-path",
                String::new(),
            ),
            (
                FieldKey::ExportRate,
                "processing-export-rate",
                String::new(),
            ),
            (
                FieldKey::ExportBitrate,
                "processing-export-bitrate",
                self.export.encoding.bitrate_kbps.to_string(),
            ),
            (
                FieldKey::ExportCompression,
                "processing-export-compression",
                self.export.encoding.compression.to_string(),
            ),
        ] {
            self.field(key, &self.tr(label), value, cx);
        }
    }

    fn show_export(&mut self, cx: &mut Context<Self>) {
        self.initialize_export(cx);
        self.probe_export_formats(cx);
        self.cancel_pointer(cx);
        self.close_workbench_controls();
        let _ = self.set_preset_popup(None, cx);
        self.export.generation = self.export.generation.wrapping_add(1);
        self.page = Page::Export;
        cx.notify();
    }

    fn export_monitor_snapshot(&self) -> EqualizerConfig {
        let mut snapshot = self.state.system.config.equalizer.clone();
        snapshot.enabled = true;
        snapshot
    }

    pub(super) fn open_original_export(&mut self, monitor_eq: bool, cx: &mut Context<Self>) {
        self.initialize_export(cx);
        self.export.source = Source::Original;
        self.export.track = self.input_track().cloned();
        self.export.monitor_eq = monitor_eq.then(|| self.export_monitor_snapshot());
        self.show_export(cx);
    }

    pub(super) fn open_graph_export(&mut self, target: GraphPort, cx: &mut Context<Self>) {
        self.initialize_export(cx);
        self.export.source = Source::Graph(target);
        self.export.track = self.input_track().cloned();
        self.export.monitor_eq = None;
        self.show_export(cx);
    }

    pub(super) fn open_frozen_export(
        &mut self,
        id: u64,
        target: GraphPort,
        cx: &mut Context<Self>,
    ) {
        self.initialize_export(cx);
        self.export.source = Source::Frozen { id, target };
        self.export.frozen_path = self
            .state
            .system
            .processing
            .as_ref()
            .map(|task| task.source_path.clone());
        self.export.monitor_eq = None;
        self.show_export(cx);
    }

    pub(super) fn retire_export_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut blur = false;
        for (key, field) in &mut self.fields {
            let visible = match key {
                FieldKey::ExportPath => self.page == Page::Export,
                FieldKey::ExportRate => self.page == Page::Export && self.export.advanced,
                FieldKey::ExportBitrate => {
                    self.page == Page::Export
                        && matches!(
                            self.export.encoding.format,
                            AudioExportFormat::Opus | AudioExportFormat::Mp3
                        )
                }
                FieldKey::ExportCompression => {
                    self.page == Page::Export
                        && self.export.encoding.format == AudioExportFormat::Flac
                }
                _ => continue,
            };
            if !visible {
                blur |= field.input.read(cx).is_focused(window);
                field.retire(cx);
            }
        }
        if blur {
            window.blur(cx);
        }
    }

    fn choose_export_path(&mut self, panel: u64, cx: &mut Context<Self>) {
        if self.choosing_path.is_some() {
            return;
        }
        let field = &self.fields[&FieldKey::ExportPath];
        let original = field.input.read(cx).text().to_owned();
        let generation = field.generation;
        let source_generation = self.export.generation;
        let request = self.begin_path_request(panel, FieldKey::ExportPath);
        let i18n = Arc::clone(&self.context.i18n);
        cx.spawn(async move |this, cx| {
            let result = ashpd::desktop::file_chooser::SelectedFiles::save_file()
                .title(i18n.text("processing-result-choose-path"))
                .accept_label(i18n.text("processing-result-choose-path"))
                .modal(true)
                .send()
                .await
                .and_then(|request| request.response());
            let _ = this.update(cx, |this, cx| {
                if !retire_path_request(&mut this.choosing_path, request) {
                    return;
                }
                cx.notify();
                if this.page != Page::Export
                    || this.panel != Some(panel)
                    || this.export.generation != source_generation
                    || !this.fields.get(&FieldKey::ExportPath).is_some_and(|field| {
                        submitted_text_current(
                            field.generation,
                            generation,
                            field.input.read(cx).text(),
                            &original,
                        )
                    })
                {
                    return;
                }
                match crate::gui::chooser_result(result) {
                    Ok(Some(files)) => {
                        if let Some(path) = crate::gui::chooser_path(&files) {
                            match path.into_os_string().into_string() {
                                Ok(path) => this.stage_graph_text(FieldKey::ExportPath, path, cx),
                                Err(_) => {
                                    this.error =
                                        Some(this.tr("processing-result-path-encoding").to_string())
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn export_request(&self, cx: &App) -> Result<AudioExportRequest, String> {
        let source = export_source(
            self.export.source,
            self.export.track.as_ref(),
            self.state.system.config.processing_graph.revision,
        )
        .ok_or_else(|| self.tr("processing-source-required").to_string())?;
        let path = self.text(FieldKey::ExportPath, cx);
        if path.is_empty() {
            return Err(self.tr("processing-graph-export-required").to_string());
        }
        let invalid = || self.tr("processing-export-invalid-options").to_string();
        let mut encoding = self.export.encoding.clone();
        let rate = self.text(FieldKey::ExportRate, cx);
        encoding.sample_rate = if rate.is_empty() {
            None
        } else {
            Some(rate.parse().map_err(|_| invalid())?)
        };
        if matches!(
            encoding.format,
            AudioExportFormat::Opus | AudioExportFormat::Mp3
        ) {
            encoding.bitrate_kbps = self
                .text(FieldKey::ExportBitrate, cx)
                .parse()
                .map_err(|_| invalid())?;
        }
        if encoding.format == AudioExportFormat::Flac {
            encoding.compression = self
                .text(FieldKey::ExportCompression, cx)
                .parse()
                .map_err(|_| invalid())?;
        }
        encoding.validate().map_err(|error| error.to_string())?;
        Ok(AudioExportRequest {
            source,
            path: path.into(),
            encoding,
            monitor_eq: self.export.monitor_eq.clone(),
        })
    }

    fn start_export(&mut self, cx: &mut Context<Self>) {
        if self.export_busy() || !self.export_format_available(self.export.encoding.format) {
            return;
        }
        if self.fields.iter().any(|(key, field)| {
            matches!(
                key,
                FieldKey::ExportPath
                    | FieldKey::ExportRate
                    | FieldKey::ExportBitrate
                    | FieldKey::ExportCompression
            ) && field.input.read(cx).is_composing()
        }) {
            return;
        }
        match self.export_request(cx) {
            Ok(request) => self.send(Command::AudioExport { request }, cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn export_busy(&self) -> bool {
        self.pending > 0
            || self
                .state
                .system
                .processing
                .as_ref()
                .is_some_and(|task| !task.phase.is_terminal())
    }

    pub(super) fn export_controls(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let unavailable = !self.export_format_available(self.export.encoding.format);
        let missing_source = export_source(
            self.export.source,
            self.export.track.as_ref(),
            self.state.system.config.processing_graph.revision,
        )
        .is_none();
        column().gap_1().child(disabled_control(
            theme,
            crate::gui::appearance::material::button(
                ("audio-export-start", panel),
                self.tr("processing-export-start"),
                cx,
                |this, _, cx| this.start_export(cx),
            )
            .debug_selector(move || format!("audio-export-start-{panel}"))
            .self_start(),
            self.export_busy() || unavailable || missing_source,
        ))
    }

    pub(super) fn export_receipt_settings(&self, request: &AudioExportRequest, cx: &App) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let source = match &request.source {
            AudioExportSource::Original { track_id } => {
                format!("{} · #{track_id}", self.tr("processing-export-original"))
            }
            AudioExportSource::Graph {
                track_id,
                target,
                revision,
            } => format!(
                "{} · #{track_id} · #{}:{} · {} {revision}",
                self.tr("processing-export-graph"),
                target.node,
                target.port,
                self.tr("processing-export-revision")
            ),
            AudioExportSource::Frozen { id, target } => format!(
                "{} #{id} · #{}:{}",
                self.tr("processing-export-frozen"),
                target.node,
                target.port
            ),
        };
        let encoding = &request.encoding;
        let quality = match encoding.format {
            AudioExportFormat::Wav if encoding.pcm_bits == 32 => {
                self.tr("processing-export-float32").to_string()
            }
            AudioExportFormat::Wav => format!("{}-bit", encoding.pcm_bits),
            AudioExportFormat::Flac => format!(
                "{}-bit · {}: {}",
                encoding.pcm_bits,
                self.tr("processing-export-compression"),
                encoding.compression
            ),
            AudioExportFormat::Opus | AudioExportFormat::Mp3 => {
                format!("{} kb/s", encoding.bitrate_kbps)
            }
        };
        let rate = encoding
            .sample_rate
            .map(|rate| format!("{rate} Hz"))
            .unwrap_or_else(|| self.tr("processing-export-keep-source").to_string());
        let channels = encoding
            .channels
            .map(|channels| channels.to_string())
            .unwrap_or_else(|| self.tr("processing-export-keep-source").to_string());
        column()
            .gap_1()
            .child(caption(theme, source))
            .child(caption(
                theme,
                format!("{} · {quality}", format_name(encoding.format)),
            ))
            .child(caption(
                theme,
                format!(
                    "{}: {rate} · {}: {channels}",
                    self.tr("processing-export-rate"),
                    self.tr("processing-export-channels")
                ),
            ))
            .child(caption(
                theme,
                format!(
                    "{}: {}",
                    self.tr("processing-export-monitor"),
                    self.tr(
                        if request.monitor_eq.as_ref().is_some_and(|eq| eq.enabled) {
                            "processing-enabled"
                        } else {
                            "processing-off"
                        }
                    )
                ),
            ))
    }

    pub(super) fn export_view(&mut self, panel: u64, cx: &mut Context<Self>) -> Div {
        self.initialize_export(cx);
        let theme = rivu_ui::theme::palette(cx);
        let mut sources = row().flex_wrap().gap_1();
        let graph_output = self
            .state
            .system
            .config
            .processing_graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, GraphNodeKind::Output))
            .map(|node| GraphPort {
                node: node.id,
                port: 0,
            });
        for (graph, label) in [
            (false, "processing-export-original"),
            (true, "processing-export-graph"),
        ] {
            let selected = if graph {
                matches!(self.export.source, Source::Graph(_))
            } else {
                self.export.source == Source::Original
            };
            sources = sources.child(disabled_control(
                theme,
                crate::gui::appearance::material::button(
                    SharedString::from(format!("audio-export-source-{panel}-{graph}")),
                    self.tr(label),
                    cx,
                    move |this, _, cx| {
                        if selected {
                            return;
                        }
                        if graph {
                            if let Some(target) = graph_output {
                                this.export.source = Source::Graph(target);
                            }
                        } else {
                            this.export.source = Source::Original;
                        }
                        this.export.generation = this.export.generation.wrapping_add(1);
                        cx.notify();
                    },
                )
                .debug_selector(move || format!("audio-export-source-{panel}-{graph}"))
                .when(selected, |view| {
                    view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                }),
                graph && graph_output.is_none() && !selected,
            ));
        }
        let mut source = column().min_w_0().gap_2().child(sources);
        match self.export.source {
            Source::Frozen { id, target } => {
                source = source.child(caption(
                    theme,
                    format!(
                        "{} #{id} · #{}:{}",
                        self.tr("processing-export-frozen"),
                        target.node,
                        target.port
                    ),
                ));
                if let Some(path) = &self.export.frozen_path {
                    source = source
                        .child(caption(theme, path.display().to_string()).overflow_x_hidden());
                }
                source = source.child(caption(theme, self.tr("processing-export-frozen-help")));
            }
            Source::Original | Source::Graph(_) => {
                if let Some(track) = &self.export.track {
                    source = source
                        .child(caption(
                            theme,
                            format!(
                                "{}: {} · #{}",
                                self.tr("processing-export-pinned"),
                                track.title,
                                track.id
                            ),
                        ))
                        .child(
                            caption(theme, track.path.display().to_string()).overflow_x_hidden(),
                        );
                } else {
                    source = source.child(caption(theme, self.tr("processing-source-required")));
                }
                if self.input_track().map(|track| track.id)
                    != self.export.track.as_ref().map(|track| track.id)
                {
                    source = source.child(disabled_control(
                        theme,
                        crate::gui::appearance::material::button(
                            ("audio-export-pin", panel),
                            self.tr("processing-export-pin-current"),
                            cx,
                            |this, _, cx| {
                                this.export.track = this.input_track().cloned();
                                this.export.generation = this.export.generation.wrapping_add(1);
                                cx.notify();
                            },
                        )
                        .debug_selector(move || format!("audio-export-pin-{panel}"))
                        .self_start(),
                        self.input_track().is_none(),
                    ));
                }
                if let Source::Graph(target) = self.export.source {
                    source = source.child(caption(
                        theme,
                        format!(
                            "{} #{}:{} · {} {}",
                            self.tr("processing-result-target"),
                            target.node,
                            target.port,
                            self.tr("processing-export-revision"),
                            self.state.system.config.processing_graph.revision
                        ),
                    ));
                    let selected = self.graph_editor.selected_output();
                    source = source.child(disabled_control(
                        theme,
                        crate::gui::appearance::material::button(
                            ("audio-export-use-port", panel),
                            self.tr("processing-export-use-port"),
                            cx,
                            move |this, _, cx| {
                                if let Some(target) = selected {
                                    this.export.source = Source::Graph(target);
                                    this.export.generation = this.export.generation.wrapping_add(1);
                                    cx.notify();
                                }
                            },
                        )
                        .self_start(),
                        selected.is_none(),
                    ));
                    source = source.child(caption(
                        theme,
                        self.tr("processing-workspace-run-committed"),
                    ));
                }
            }
        }
        let has_eq = self.export.monitor_eq.is_some();
        let mut eq = column()
            .gap_1()
            .child(
                crate::gui::appearance::material::button(
                    ("audio-export-eq", panel),
                    self.tr(if has_eq {
                        "processing-export-eq-on"
                    } else {
                        "processing-export-eq-off"
                    }),
                    cx,
                    |this, _, cx| {
                        this.export.monitor_eq = if this.export.monitor_eq.is_some() {
                            None
                        } else {
                            Some(this.export_monitor_snapshot())
                        };
                        cx.notify();
                    },
                )
                .debug_selector(move || format!("audio-export-eq-{panel}"))
                .self_start()
                .when(has_eq, |view| {
                    view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                }),
            )
            .child(caption(theme, self.tr("processing-export-eq-help")));
        if has_eq {
            eq = eq.child(
                crate::gui::appearance::material::button(
                    ("audio-export-refresh-eq", panel),
                    self.tr("processing-export-refresh-eq"),
                    cx,
                    |this, _, cx| {
                        this.export.monitor_eq = Some(this.export_monitor_snapshot());
                        cx.notify();
                    },
                )
                .self_start(),
            );
        }
        let mut formats = row().flex_wrap().gap_1();
        for format in [
            AudioExportFormat::Wav,
            AudioExportFormat::Flac,
            AudioExportFormat::Opus,
            AudioExportFormat::Mp3,
        ] {
            let available = self.export_format_available(format);
            let name = format_name(format);
            formats = formats.child(disabled_control(
                theme,
                crate::gui::appearance::material::button(
                    SharedString::from(format!("audio-export-format-{panel}-{name}")),
                    name,
                    cx,
                    move |this, _, cx| {
                        this.export.encoding.format = format;
                        if format == AudioExportFormat::Flac && this.export.encoding.pcm_bits == 32
                        {
                            this.export.encoding.pcm_bits = 24;
                        }
                        cx.notify();
                    },
                )
                .debug_selector(move || format!("audio-export-format-{panel}-{name}"))
                .when(self.export.encoding.format == format, |view| {
                    view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                }),
                !available,
            ));
        }
        let mut encoding = column().gap_2().child(formats);
        if self.state.system.export_formats.is_none() {
            encoding = encoding.child(caption(theme, self.tr("processing-export-codec-checking")));
        } else if self
            .state
            .system
            .export_formats
            .as_ref()
            .is_some_and(|formats| formats.len() < 4)
        {
            encoding = encoding.child(caption(
                theme,
                self.tr("processing-export-codec-unavailable"),
            ));
        }
        match self.export.encoding.format {
            AudioExportFormat::Wav | AudioExportFormat::Flac => {
                let mut bits = row().flex_wrap().gap_1();
                for value in [16, 24, 32] {
                    if value == 32 && self.export.encoding.format == AudioExportFormat::Flac {
                        continue;
                    }
                    bits = bits.child(
                        crate::gui::appearance::material::button(
                            SharedString::from(format!("audio-export-bits-{panel}-{value}")),
                            if value == 32 {
                                self.tr("processing-export-float32")
                            } else {
                                format!("{value}-bit").into()
                            },
                            cx,
                            move |this, _, cx| {
                                this.export.encoding.pcm_bits = value;
                                cx.notify();
                            },
                        )
                        .debug_selector(move || format!("audio-export-bits-{panel}-{value}"))
                        .when(self.export.encoding.pcm_bits == value, |view| {
                            view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                        }),
                    );
                }
                encoding = encoding.child(Self::parameter_row(
                    caption(theme, self.tr("processing-export-depth")),
                    bits,
                ));
                if self.export.encoding.format == AudioExportFormat::Flac {
                    encoding = encoding.child(
                        self.field_view(
                            panel,
                            FieldKey::ExportCompression,
                            self.tr("processing-export-compression"),
                            cx,
                        )
                        .id(("audio-export-compression", panel))
                        .debug_selector(move || format!("audio-export-compression-{panel}")),
                    );
                }
            }
            AudioExportFormat::Opus | AudioExportFormat::Mp3 => {
                encoding = encoding.child(
                    self.field_view(
                        panel,
                        FieldKey::ExportBitrate,
                        self.tr("processing-export-bitrate"),
                        cx,
                    )
                    .id(("audio-export-bitrate", panel))
                    .debug_selector(move || format!("audio-export-bitrate-{panel}")),
                );
                encoding = encoding.child(caption(
                    theme,
                    self.tr(if self.export.encoding.format == AudioExportFormat::Opus {
                        "processing-export-opus-rate"
                    } else {
                        "processing-export-mp3-rate"
                    }),
                ));
            }
        }
        let path_label = self.tr("processing-export-path");
        let path_input = self.field_input(panel, FieldKey::ExportPath, &path_label, cx);
        let choose_path = IconButton::new(
            ("audio-export-choose", panel),
            "",
            self.tr("processing-result-choose-path"),
        )
        .surface(crate::gui::appearance::material::surface(cx))
        .build(
            IconButtonStyle::Quiet,
            IconButtonDensity::Compact,
            cx,
            move |this, _, cx| this.choose_export_path(panel, cx),
        )
        .debug_selector(move || format!("audio-export-choose-{panel}"))
        .flex_shrink_0();
        let destination = column()
            .gap_1()
            .child(
                Self::parameter_row(
                    caption(theme, path_label),
                    row()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(path_input))
                        .child(choose_path),
                )
                .id(("audio-export-path", panel))
                .debug_selector(move || format!("audio-export-path-{panel}")),
            )
            .child(caption(
                theme,
                self.tr("processing-export-destination-help"),
            ));
        let mut advanced = column().gap_2().child(
            activate_control(
                dropdown_trigger(
                    theme,
                    ("audio-export-advanced", panel),
                    self.tr("processing-export-advanced"),
                    self.export.advanced,
                    cx,
                ),
                cx,
                |this, _, cx| {
                    this.export.advanced = !this.export.advanced;
                    cx.notify();
                },
            )
            .debug_selector(move || format!("audio-export-advanced-{panel}")),
        );
        if self.export.advanced {
            let mut channels = row().flex_wrap().gap_1();
            for (value, label) in [
                (None, "processing-export-keep-source"),
                (Some(1), "processing-export-mono"),
                (Some(2), "processing-export-stereo"),
            ] {
                channels = channels.child(
                    crate::gui::appearance::material::button(
                        SharedString::from(format!("audio-export-channels-{panel}-{value:?}")),
                        self.tr(label),
                        cx,
                        move |this, _, cx| {
                            this.export.encoding.channels = value;
                            cx.notify();
                        },
                    )
                    .debug_selector(move || format!("audio-export-channels-{panel}-{value:?}"))
                    .when(self.export.encoding.channels == value, |view| {
                        view.bg(rgb(theme.selected)).text_color(rgb(theme.accent))
                    }),
                );
            }
            advanced = advanced
                .child(
                    self.field_view(
                        panel,
                        FieldKey::ExportRate,
                        self.tr("processing-export-rate"),
                        cx,
                    )
                    .id(("audio-export-rate", panel))
                    .debug_selector(move || format!("audio-export-rate-{panel}")),
                )
                .child(caption(theme, self.tr("processing-export-rate-help")))
                .child(Self::parameter_row(
                    caption(theme, self.tr("processing-export-channels")),
                    channels,
                ));
        }
        let mut body = column()
            .min_w_0()
            .gap_3()
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-export-source"),
                source,
            ))
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-export-monitor"),
                eq,
            ))
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-export-encoding"),
                encoding,
            ))
            .child(rivu_ui::section_group(
                theme,
                self.tr("processing-export-destination"),
                destination,
            ))
            .child(advanced);
        if let Some(task) = &self.state.system.processing
            && task.settings.audio_export.is_some()
        {
            if let Some(path) = &task.graph_exported_path {
                body = body.child(caption(
                    theme,
                    format!(
                        "{}: {}",
                        self.tr("processing-graph-exported"),
                        path.display()
                    ),
                ));
            }
            if let Some(error) = &task.error {
                body = body.child(caption(theme, error.clone()).text_color(rgb(theme.error)));
            }
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[gpui::test]
    fn export_pins_track_and_explicit_eq_without_following_playback(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(Input::init);
        let (_directory, _runtime, owner) = test_support::owner(cx, test_support::config());
        owner.update(cx, |this, cx| {
            this.command_active = true;
            let mut track = crate::test_support::artwork_track("source.wav".into());
            track.id = 41;
            Rc::make_mut(&mut this.state).current_track = Some(Arc::new(track.clone()));
            Rc::make_mut(&mut this.state).system.export_formats =
                Some(vec![AudioExportFormat::Wav]);
            this.open_original_export(true, cx);
            this.stage_graph_text(FieldKey::ExportPath, " result.wav ".into(), cx);
            let captured_eq = this.export.monitor_eq.clone();
            track.id = 42;
            Rc::make_mut(&mut this.state).current_track = Some(Arc::new(track));
            Arc::make_mut(&mut Rc::make_mut(&mut this.state).system.config)
                .equalizer
                .enabled = false;
            let request = this.export_request(cx).unwrap();
            assert_eq!(request.source, AudioExportSource::Original { track_id: 41 });
            assert_eq!(request.path, PathBuf::from(" result.wav "));
            assert_eq!(request.monitor_eq, captured_eq);
            assert!(request.monitor_eq.as_ref().unwrap().enabled);
            assert!(request.encoding.sample_rate.is_none());
            assert!(request.encoding.channels.is_none());
            this.start_export(cx);
            let Command::AudioExport { request } = this.commands.pop_front().unwrap().command
            else {
                panic!("expected an export request");
            };
            assert_eq!(request.source, AudioExportSource::Original { track_id: 41 });
        });
    }

    #[gpui::test]
    fn graph_export_captures_port_revision_and_does_not_add_monitor_eq(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(Input::init);
        let (_directory, _runtime, owner) = test_support::owner(cx, test_support::config());
        owner.update(cx, |this, cx| {
            this.command_active = true;
            let mut track = crate::test_support::artwork_track("source.wav".into());
            track.id = 7;
            Rc::make_mut(&mut this.state).current_track = Some(Arc::new(track));
            Rc::make_mut(&mut this.state).system.export_formats =
                Some(vec![AudioExportFormat::Wav]);
            let target = GraphPort { node: 3, port: 1 };
            this.open_graph_export(target, cx);
            this.stage_graph_text(FieldKey::ExportPath, "out.wav".into(), cx);
            this.stage_graph_text(FieldKey::ExportBitrate, "irrelevant".into(), cx);
            this.stage_graph_text(FieldKey::ExportCompression, "irrelevant".into(), cx);
            Arc::make_mut(&mut Rc::make_mut(&mut this.state).system.config)
                .processing_graph
                .revision = 9;
            let request = this.export_request(cx).unwrap();
            assert_eq!(
                request.source,
                AudioExportSource::Graph {
                    track_id: 7,
                    target,
                    revision: 9
                }
            );
            assert!(request.monitor_eq.is_none());
            this.stage_graph_text(FieldKey::ExportRate, "44100".into(), cx);
            this.export.encoding.channels = Some(1);
            let request = this.export_request(cx).unwrap();
            assert_eq!(request.encoding.sample_rate, Some(44_100));
            assert_eq!(request.encoding.channels, Some(1));
            this.export.encoding.format = AudioExportFormat::Mp3;
            assert!(this.export_request(cx).is_err());
            this.stage_graph_text(FieldKey::ExportBitrate, "192".into(), cx);
            assert_eq!(this.export_request(cx).unwrap().encoding.bitrate_kbps, 192);
        });
    }

    #[gpui::test]
    fn encoder_discovery_recovers_after_queue_rejection(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(Input::init);
        let (_directory, _runtime, owner) = test_support::owner(cx, test_support::config());
        owner.update(cx, |this, cx| {
            this.command_active = true;
            this.initialize_export(cx);
            assert!(
                this.commands.is_empty(),
                "draft setup/render does not load encoders"
            );
            this.pending = 64;
            this.probe_export_formats(cx);
            assert!(this.commands.is_empty());
            this.pending = 0;
            this.probe_export_formats(cx);
            assert_eq!(this.commands.len(), 1);
            assert!(matches!(
                this.commands.pop_front().unwrap().command,
                Command::ProbeAudioExportFormats
            ));
            this.pending = 0;
            assert!(!this.export_format_available(AudioExportFormat::Mp3));
            Rc::make_mut(&mut this.state).system.export_formats =
                Some(vec![AudioExportFormat::Wav]);
            assert!(this.export_format_available(AudioExportFormat::Wav));
            assert!(!this.export_format_available(AudioExportFormat::Mp3));
            this.export.encoding.format = AudioExportFormat::Mp3;
            this.start_export(cx);
            assert!(
                this.commands.is_empty(),
                "unavailable codec cannot enqueue work"
            );
        });
    }

    struct ExportWindow(Entity<AudioProcessing>);

    impl Render for ExportWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.0
                .update(cx, |this, cx| this.render_panel(7, window, cx))
        }
    }

    fn draw(cx: &mut TestAppContext, handle: AnyWindowHandle) {
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    fn click(cx: &mut TestAppContext, handle: AnyWindowHandle, selector: &'static str) {
        use gpui::InputEvent as _;
        let position = VisualTestContext::from_window(handle, cx)
            .debug_bounds(selector)
            .unwrap()
            .center();
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })
        .unwrap();
        draw(cx, handle);
    }

    #[gpui::test]
    fn native_export_controls_preserve_drafts_and_submit_selected_format(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(Input::init);
        let (_directory, _runtime, owner) = test_support::owner(cx, test_support::config());
        owner.update(cx, |this, cx| {
            this.command_active = true;
            Rc::make_mut(&mut this.state).current_track = Some(Arc::new(
                crate::test_support::artwork_track("source.wav".into()),
            ));
            Rc::make_mut(&mut this.state).system.export_formats =
                Some(vec![AudioExportFormat::Wav, AudioExportFormat::Mp3]);
            this.open_original_export(false, cx);
        });
        let window = cx.open_window(size(px(1200.), px(1000.)), {
            let owner = owner.clone();
            move |_, _| ExportWindow(owner)
        });
        let handle = window.into();
        draw(cx, handle);
        let input = owner.read_with(cx, |this, _| {
            this.fields[&FieldKey::ExportPath].input.clone()
        });
        cx.update_window(handle, |_, window, cx| {
            input.read(cx).focus_handle(cx).focus(window, cx)
        })
        .unwrap();
        for key in ["o", "u", "t", ".", "m", "p", "3"] {
            cx.update_window(handle, |_, window, cx| {
                window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx)
            })
            .unwrap();
        }
        draw(cx, handle);
        click(cx, handle, "audio-export-format-7-mp3");
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("audio-export-bitrate-7")
                .is_some()
        );
        assert!(
            VisualTestContext::from_window(handle, cx)
                .debug_bounds("audio-export-compression-7")
                .is_none()
        );
        owner.read_with(cx, |this, cx| {
            assert_eq!(this.text(FieldKey::ExportPath, cx), "out.mp3")
        });
        click(cx, handle, "audio-export-format-7-flac");
        owner.read_with(cx, |this, _| {
            assert_eq!(this.export.encoding.format, AudioExportFormat::Mp3)
        });
        click(cx, handle, "audio-export-eq-7");
        click(cx, handle, "audio-export-start-7");
        owner.update(cx, |this, _| {
            let Command::AudioExport { request } = this.commands.pop_front().unwrap().command
            else {
                panic!("expected native Start to enqueue export");
            };
            assert_eq!(request.path, PathBuf::from("out.mp3"));
            assert_eq!(request.encoding.format, AudioExportFormat::Mp3);
            assert!(request.monitor_eq.as_ref().unwrap().enabled);
            assert_eq!(this.pending, 1);
        });
        click(cx, handle, "audio-export-start-7");
        owner.read_with(cx, |this, _| {
            assert!(
                this.commands.is_empty(),
                "pending export disables duplicate submission"
            )
        });
        click(cx, handle, "processing-page-7-processing-tab-results");
        cx.update_window(handle, |_, window, cx| {
            assert!(!input.read(cx).is_focused(window))
        })
        .unwrap();
    }
}
