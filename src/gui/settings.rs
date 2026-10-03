use super::{
    BORDER, GuiApp, button, column,
    input::Input,
    panels::{TRACK_HEIGHT, caption, list_row},
    row,
};
use crate::{
    config::Config,
    model::{Command, RepeatMode},
};
use anyhow::Result;
use gpui::{prelude::*, *};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Roots,
    Device,
    Volume,
    Scale,
    Fps,
}

/// Owns unsaved preferences and their text inputs, independently of live app state.
/// Refreshing playback snapshots must not overwrite edits; only reload/discard does.
pub(super) struct Settings {
    draft: Config,
    inputs: HashMap<Field, Entity<Input>>,
}

impl Settings {
    pub(super) fn new(config: &Config, cx: &mut App) -> Self {
        let mut inputs = HashMap::new();
        for (field, placeholder) in [
            (Field::Roots, "Library roots separated by ;"),
            (Field::Device, "Default output"),
            (Field::Volume, "Volume 0–100"),
            (Field::Scale, "Interface scale 0.75–2"),
            (Field::Fps, "Analysis frames/s 5–60"),
        ] {
            inputs.insert(field, cx.new(|cx| Input::new("", placeholder, cx)));
        }
        let mut settings = Self {
            draft: config.clone(),
            inputs,
        };
        settings.fill(cx);
        settings
    }

    fn field(&self, field: Field, label: &'static str) -> Div {
        column()
            .w_full()
            .flex_shrink_0()
            .gap_1()
            .child(caption(label))
            .child(self.inputs[&field].clone())
    }

    fn value(&self, field: Field, cx: &App) -> String {
        self.inputs[&field].read(cx).text().to_owned()
    }

    fn set_value(&mut self, field: Field, value: impl Into<SharedString>, cx: &mut App) {
        let value = value.into();
        self.inputs[&field].update(cx, |input, cx| input.set_text(value, cx));
    }

    fn reset(&mut self, config: &Config, cx: &mut App) {
        self.draft = config.clone();
        self.fill(cx);
    }

    fn fill(&mut self, cx: &mut App) {
        self.set_value(
            Field::Roots,
            self.draft
                .library_roots
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(";"),
            cx,
        );
        self.set_value(
            Field::Device,
            self.draft.output_device.clone().unwrap_or_default(),
            cx,
        );
        self.set_value(
            Field::Volume,
            format!("{:.0}", self.draft.volume * 100.),
            cx,
        );
        self.set_value(Field::Scale, self.draft.ui_scale.to_string(), cx);
        self.set_value(Field::Fps, self.draft.analysis_fps.to_string(), cx);
    }

    fn parse(&self, cx: &App) -> Result<Config> {
        let mut config = self.draft.clone();
        config.library_roots = self
            .value(Field::Roots, cx)
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect();
        let device = self.value(Field::Device, cx);
        config.output_device = (!device.trim().is_empty()).then(|| device.trim().to_owned());
        config.volume = self.value(Field::Volume, cx).trim().parse::<f32>()? / 100.;
        config.ui_scale = self.value(Field::Scale, cx).trim().parse()?;
        config.analysis_fps = self.value(Field::Fps, cx).trim().parse()?;
        config.validate()?;
        Ok(config)
    }
}

impl GuiApp {
    pub(super) fn load_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.reset(&self.state.config, cx);
    }

    fn reload_settings(&mut self, cx: &mut Context<Self>) {
        match Config::load(&self.state.config_path) {
            Ok(config) => {
                self.settings.reset(&config, cx);
                self.send(Command::Configure { config }, cx);
            }
            Err(error) => {
                self.error = Some(format!("Reloading settings: {error:#}"));
                cx.notify();
            }
        }
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        match self.settings.parse(cx) {
            Ok(config) => {
                self.settings.draft = config.clone();
                self.send(Command::Configure { config }, cx);
            }
            Err(error) => {
                self.error = Some(format!("Invalid settings: {error:#}"));
                cx.notify();
            }
        }
    }
}

impl GuiApp {
    pub(super) fn settings_panel(
        &mut self,
        panel_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shuffle = if self.settings.draft.shuffle {
            "Shuffle: on"
        } else {
            "Shuffle: off"
        };
        let repeat = match self.settings.draft.repeat {
            RepeatMode::Off => "Repeat: off",
            RepeatMode::All => "Repeat: all",
            RepeatMode::One => "Repeat: one",
        };
        let mpris = if self.settings.draft.mpris_enabled {
            "MPRIS: enabled"
        } else {
            "MPRIS: disabled"
        };
        let device_height = ((self.state.devices.len() + 1) as f32 * TRACK_HEIGHT).min(126.0);
        let chosen_device = self.settings.value(Field::Device, cx);
        column()
            .id(("settings-panel", panel_id))
            .size_full()
            .p_3()
            .overflow_y_scroll()
            .child(caption("Preferences · changes take effect when saved"))
            .child(caption(self.state.config_path.to_string_lossy().into_owned()).truncate())
            .child(
                row()
                    .flex_wrap()
                    .flex_shrink_0()
                    .child(button(
                        ("settings-save", panel_id),
                        "Save & apply",
                        cx,
                        |this, _, cx| this.apply_settings(cx),
                    ))
                    .child(button(
                        ("settings-reload", panel_id),
                        "Reload file",
                        cx,
                        |this, _, cx| this.reload_settings(cx),
                    ))
                    .child(button(
                        ("settings-revert", panel_id),
                        "Discard edits",
                        cx,
                        |this, _, cx| this.load_settings(cx),
                    )),
            )
            .child(self.settings.field(
                Field::Roots,
                "Library roots (separate paths with semicolons)",
            ))
            .child(
                self.settings
                    .field(Field::Device, "Output device (blank = system default)"),
            )
            .child(caption("Available outputs · click to select"))
            .child(
                uniform_list(
                    ("settings-devices", panel_id),
                    self.state.devices.len() + 1,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let name = if index == 0 {
                                    String::new()
                                } else {
                                    this.state.devices.get(index - 1)?.clone()
                                };
                                let label = if name.is_empty() {
                                    "System default".to_owned()
                                } else {
                                    name.clone()
                                };
                                let selected = chosen_device == name;
                                Some(
                                    list_row(("output-device", index), selected)
                                        .child(div().text_sm().truncate().child(label))
                                        .on_click(cx.listener(
                                            move |this, _: &gpui::ClickEvent, _, cx| {
                                                this.settings.draft.output_device =
                                                    (!name.is_empty()).then(|| name.clone());
                                                this.settings.set_value(
                                                    Field::Device,
                                                    name.clone(),
                                                    cx,
                                                );
                                                cx.notify();
                                            },
                                        )),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(device_height))
                .flex_shrink_0()
                .w_full(),
            )
            .child(self.settings.field(Field::Volume, "Volume (0–100%)"))
            .child(
                row()
                    .flex_wrap()
                    .flex_shrink_0()
                    .child(button(
                        ("settings-shuffle", panel_id),
                        shuffle,
                        cx,
                        |this, _, cx| {
                            this.settings.draft.shuffle = !this.settings.draft.shuffle;
                            cx.notify();
                        },
                    ))
                    .child(button(
                        ("settings-repeat", panel_id),
                        repeat,
                        cx,
                        |this, _, cx| {
                            this.settings.draft.repeat = match this.settings.draft.repeat {
                                RepeatMode::Off => RepeatMode::All,
                                RepeatMode::All => RepeatMode::One,
                                RepeatMode::One => RepeatMode::Off,
                            };
                            cx.notify();
                        },
                    )),
            )
            .child(
                self.settings
                    .field(Field::Scale, "Interface scale (0.75–2.0)"),
            )
            .child(
                self.settings
                    .field(Field::Fps, "Analysis frames / second (5–60)"),
            )
            .child(row().child(button(
                ("settings-mpris", panel_id),
                mpris,
                cx,
                |this, _, cx| {
                    this.settings.draft.mpris_enabled = !this.settings.draft.mpris_enabled;
                    cx.notify();
                },
            )))
            .child(caption(format!(
                "Media controls: {}",
                self.state.mpris_status
            )))
            .child(div().h(px(1.0)).flex_shrink_0().bg(rgb(BORDER)))
            .child(row().flex_wrap().child(button(
                ("settings-reset-workspace", panel_id),
                "Reset workspace",
                cx,
                |this, _, cx| {
                    this.layout = super::layout::Layout::default();
                    this.persist_layout(cx);
                    cx.notify();
                },
            )))
            .child(caption(
                "Resets panel placement only. Your music and preferences are kept.",
            ))
            .into_any_element()
    }
}
