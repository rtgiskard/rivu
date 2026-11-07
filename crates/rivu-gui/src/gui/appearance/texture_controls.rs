//! Live, automatically persisted material inspector. The shell owns attachment,
//! focus and preset choice state; intents reconcile through window coordination.
use super::material::{
    Material, MaterialGroup, MaterialIntent, MaterialSession, Parameter, TextureSettings,
};
use gpui::{prelude::*, *};
use rivu_ui::{
    ChoiceItem, ChoicePopup, ChoiceSelect, Dialog, FocusScope, IconButton, IconButtonDensity,
    IconButtonStyle, caption, column,
    input::{Input, InputEvent},
    label_value_row, row,
};

pub(in crate::gui) struct MaterialEdit {
    material: Material,
    parameter: Parameter,
    serial: u64,
    input: Entity<Input>,
    error: Option<String>,
    _submit: Subscription,
    _blur: Subscription,
}

impl crate::gui::WindowView {
    pub(in crate::gui) fn material_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.shell
            .material_edit
            .as_ref()
            .is_some_and(|edit| edit.input.read(cx).is_focused(window))
    }

    fn begin_material_edit(
        &mut self,
        material: Material,
        parameter: Parameter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = MaterialSession::current(cx);
        if !session.enabled || session.material != material {
            return;
        }
        self.shell.material_choices.close();
        self.shell.material_edit = None;
        self.shell.material_edit_serial = self.shell.material_edit_serial.wrapping_add(1);
        let serial = self.shell.material_edit_serial;
        let value =
            parameter.value(MaterialSession::current(cx).settings) * parameter.edit_scale(material);
        let label = self.tr(parameter.label(material));
        let input = cx.new(|cx| Input::new(label, value.to_string(), "", cx).embedded());
        let submit = cx.subscribe(&input, move |this, _, event, cx| {
            if !this
                .shell
                .material_edit
                .as_ref()
                .is_some_and(|edit| edit.serial == serial)
            {
                return;
            }
            match event {
                InputEvent::Submit => this.submit_material_edit(cx),
                InputEvent::Changed { user: true, .. } => {
                    if let Some(edit) = this.shell.material_edit.as_mut() {
                        edit.error = None;
                    }
                    cx.notify();
                }
                _ => {}
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&focus, window, move |this, _, cx| {
            if this
                .shell
                .material_edit
                .as_ref()
                .is_some_and(|edit| edit.serial == serial)
            {
                this.shell.material_edit = None;
                cx.notify();
            }
        });
        self.shell.material_edit = Some(MaterialEdit {
            material,
            parameter,
            serial,
            input: input.clone(),
            error: None,
            _submit: submit,
            _blur: blur,
        });
        input.update(cx, |input, cx| input.focus_and_select_all(window, cx));
        let owner = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let _ = owner.update(cx, |this, cx| {
                if this.shell.material_edit.as_ref().is_some_and(|edit| {
                    edit.serial == serial && !edit.input.read(cx).is_focused(window)
                }) {
                    this.shell.material_edit = None;
                    cx.notify();
                }
            });
        });
        cx.notify();
    }

    fn submit_material_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.shell.material_edit.as_ref() else {
            return;
        };
        let material = edit.material;
        let parameter = edit.parameter;
        let session = MaterialSession::current(cx);
        if !session.enabled || session.material != material {
            self.shell.material_edit = None;
            cx.notify();
            return;
        }
        let scale = parameter.edit_scale(material);
        let (min, max, _) = parameter.limits();
        let parsed = edit.input.read(cx).text().trim().parse::<f32>();
        let value = parsed.ok().map(|value| value / scale).filter(|value| {
            value.is_finite()
                && *value >= min
                && *value <= max
                && (!matches!(parameter, Parameter::Steps)
                    || *value == 0.
                    || (*value >= 2. && value.fract() == 0.))
        });
        let Some(value) = value else {
            let error = if matches!(parameter, Parameter::Steps) {
                self.tr("material-steps-invalid").to_string()
            } else {
                format!(
                    "{} {}–{} {}",
                    self.tr("material-value-invalid"),
                    min * scale,
                    max * scale,
                    parameter.edit_unit(material)
                )
            };
            self.shell.material_edit.as_mut().unwrap().error = Some(error);
            cx.notify();
            return;
        };
        self.shell.material_edit = None;
        self.material_intent(
            MaterialIntent::Set {
                material,
                parameter,
                value,
            },
            cx,
        );
        cx.notify();
    }
}

impl Parameter {
    fn edit_scale(self, material: Material) -> f32 {
        match self {
            Self::Alpha | Self::Feather | Self::Highlight | Self::Grain | Self::EdgeFade => 100.,
            Self::Wave if material.is_procedural_water() => 100. / 16.,
            _ => 1.,
        }
    }

    fn edit_unit(self, material: Material) -> &'static str {
        match self {
            Self::Alpha | Self::Feather | Self::Highlight | Self::Grain | Self::EdgeFade => "%",
            Self::Wave if material.is_procedural_water() => "%",
            Self::Wave | Self::CoverBlur | Self::BackdropBlur => "px",
            Self::Transition => "s",
            Self::Speed => "×",
            Self::Steps => "",
        }
    }

    fn display(
        self,
        material: Material,
        settings: TextureSettings,
        tr: &impl Fn(&str) -> SharedString,
    ) -> String {
        let value = self.value(settings);
        if material.is_procedural_water() && matches!(self, Self::Wave) {
            return format!(
                "{:.0}%",
                settings.cg_water_params(material, 0., false).amplitude * 100.
            );
        }
        match self {
            Self::Alpha | Self::Feather | Self::Highlight | Self::Grain | Self::EdgeFade => {
                format!("{:.0}%", value * 100.)
            }
            Self::Steps => {
                if value == 0. {
                    tr("texture-off").to_string()
                } else {
                    format!("{value:.0}")
                }
            }
            Self::CoverBlur | Self::BackdropBlur => format!("{value:.0} px"),
            Self::Wave => format!("{value:.1} px"),
            Self::Transition => format!("{value:.2} s"),
            Self::Speed => format!("{value:.2}×"),
        }
    }
}

fn preset_label(material: Material, tr: &impl Fn(&str) -> SharedString) -> SharedString {
    match material.group() {
        Some(group) => format!("{} · {}", tr(group.label()), tr(material.label())).into(),
        None => tr(material.label()),
    }
}

/// Attach after the workspace; the dialog exists only in the window that opened it.
#[allow(clippy::too_many_arguments)]
pub(in crate::gui) fn render(
    view: &mut crate::gui::WindowView,
    material: Material,
    experimental_expanded: bool,
    scope: Option<&FocusScope>,
    tr: impl Fn(&str) -> SharedString,
    window: &Window,
    cx: &mut Context<crate::gui::WindowView>,
) -> AnyElement {
    let theme = rivu_ui::theme::palette(cx);
    let session = MaterialSession::current(cx);
    let settings = session.settings;
    if view
        .shell
        .material_edit
        .as_ref()
        .is_some_and(|edit| edit.material != material)
    {
        view.shell.material_edit = None;
    }
    let footer_bounds = view.shell.footer_bounds.get();
    let gap = window.rem_size() * 0.375;
    let bottom_inset = footer_bounds
        .map(|bounds| (window.viewport_size().height - bounds.top() + gap).max(px(0.)))
        .unwrap_or(window.viewport_size().height);
    let right_inset = footer_bounds
        .map(|bounds| {
            (window.viewport_size().width - bounds.right() + window.rem_size() * 0.5).max(px(0.))
        })
        .unwrap_or(px(0.));
    let available_height =
        (window.viewport_size().height - bottom_inset - window.rem_size() * 0.5).max(px(0.));
    let anchor = view.shell.material_choice_anchor.clone();
    let measure = anchor.clone();
    let trigger = ChoiceSelect::new("material-preset", preset_label(material, &tr))
        .build(
            view,
            |this| Some(&mut this.shell.material_choices),
            cx,
            move |this, _, cx| {
                if !MaterialSession::current(cx).enabled {
                    return;
                }
                if this.shell.material_choices.is_open() {
                    this.shell.material_choices.close();
                } else {
                    let mut items = Vec::new();
                    for group in MaterialGroup::ALL {
                        if group == MaterialGroup::Experimental
                            && !experimental_expanded
                            && material.group() != Some(group)
                        {
                            continue;
                        }
                        for candidate in Material::ALL {
                            if candidate.group() == Some(group)
                                && (group != MaterialGroup::Experimental
                                    || experimental_expanded
                                    || candidate == material)
                            {
                                items.push(ChoiceItem::new(
                                    candidate,
                                    preset_label(candidate, &|key| this.tr(key)),
                                ));
                            }
                        }
                    }
                    let selected = items
                        .iter()
                        .position(|item| item.value == material)
                        .unwrap_or(0);
                    this.shell.material_choices.open(items, selected);
                }
                cx.notify();
            },
            crate::gui::WindowView::choose_material_preset,
            |this, _, cx| {
                this.shell.material_choices.close();
                cx.notify();
            },
        )
        .tooltip(rivu_ui::text_tooltip(preset_label(material, &tr)))
        .relative()
        .child(
            canvas(
                move |bounds, _, _| measure.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
    let trigger = rivu_ui::disabled_control(theme, trigger, !session.enabled);
    let choices = row()
        .gap_1()
        .min_w_0()
        .flex_shrink_0()
        .child(trigger.w_auto().min_w(rems(12.)).flex_shrink(1.))
        .child(
            IconButton::new(
                "material-experimental",
                "",
                tr("material-experimental-presets"),
            )
            .switch(experimental_expanded)
            .enabled(session.enabled)
            .build(
                IconButtonStyle::Quiet,
                IconButtonDensity::Compact,
                cx,
                |this, _, cx| this.toggle_experimental_materials(cx),
            )
            .w(rems(2.))
            .flex_shrink_0(),
        )
        .child(
            IconButton::new("texture-reset", "󰑐", tr("texture-reset"))
                .enabled(
                    session.enabled && (session.modified() || view.shell.material_edit.is_some()),
                )
                .build(
                    IconButtonStyle::Quiet,
                    IconButtonDensity::Compact,
                    cx,
                    move |this, _, cx| {
                        this.material_intent(MaterialIntent::Reset(material), cx);
                        this.shell.material_edit = None;
                    },
                )
                .w(rems(2.))
                .flex_shrink_0(),
        );
    let mut parameters = column().min_w_0().flex_shrink_0().gap_0();
    let label_width = material
        .spec()
        .controls()
        .map(|parameter| {
            let label = tr(parameter.label(material));
            let run = TextRun {
                len: label.len(),
                font: view.ui_font.clone(),
                color: rgb(theme.secondary).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(
                    label,
                    window.rem_size() * 0.75 * rivu_ui::theme::font_size_scale(cx),
                    &[run],
                    None,
                )
                .width()
        })
        .fold(px(0.), Pixels::max);
    let scale = window.scale_factor();
    let label_width = px((f32::from(label_width) * scale).ceil() / scale);
    let height = rivu_ui::tree_row_height(window, cx);
    for (index, parameter) in material.spec().controls().enumerate() {
        let label = tr(parameter.label(material));
        let field = if let Some(edit) = view.shell.material_edit.as_ref().filter(|edit| {
            session.enabled
                && edit.material == material
                && edit.parameter as usize == parameter as usize
        }) {
            row()
                .group("label-value-row")
                .min_w_0()
                .h(height)
                .gap_4()
                .child(
                    caption(theme, label)
                        .w(label_width)
                        .flex_shrink_0()
                        .truncate(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(rems(4.))
                        .h_full()
                        .border_1()
                        .border_color(rgb(if edit.error.is_some() {
                            theme.error
                        } else {
                            theme.accent
                        }))
                        .rounded_sm()
                        .child(edit.input.clone()),
                )
                .when(!parameter.edit_unit(material).is_empty(), |row| {
                    row.child(caption(theme, parameter.edit_unit(material)).flex_shrink_0())
                })
                .when_some(edit.error.clone(), |row, error| {
                    row.child(
                        caption(theme, "")
                            .id(("material-field-error", index))
                            .tooltip(rivu_ui::text_tooltip(error)),
                    )
                })
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                        this.shell.material_edit = None;
                        this.shell.scope().handle().focus(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
        } else {
            let actions =
                row()
                    .id(("material-field-actions", index))
                    .gap_0()
                    .child(rivu_ui::hover_reveal(
                        ("material-edit-action", index),
                        "label-value-row",
                        IconButton::new(("texture-parameter", index), "", tr("gl-edit-value"))
                            .enabled(session.enabled)
                            .build(
                                IconButtonStyle::Quiet,
                                IconButtonDensity::Compact,
                                cx,
                                move |this, window, cx| {
                                    this.begin_material_edit(material, parameter, window, cx);
                                },
                            )
                            .min_h_0()
                            .h(height)
                            .py_0()
                            .text_size(rems(0.75)),
                    ));
            label_value_row(
                theme,
                label,
                parameter.display(material, settings, &tr),
                label_width,
                Some(actions),
            )
            .flex_initial()
            .h(height)
            .items_center()
        };
        parameters = parameters.child(field.flex_shrink_0());
    }
    if material.spec().controls().next().is_none() {
        parameters = parameters.child(
            caption(theme, tr("material-fixed-info"))
                .max_w(rems(18.))
                .whitespace_normal(),
        );
    }
    let body = div()
        .id("material-controls-body")
        .flex()
        .flex_col()
        .flex_shrink(1.)
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .child(parameters);
    let content = Dialog::card(theme)
        .id("texture-controls-card")
        .w_auto()
        .max_w_full()
        .max_h(available_height)
        .overflow_hidden()
        .p_2()
        .gap_2()
        .role(Role::Dialog)
        .aria_label(tr("texture-title"))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(
            row()
                .gap_2()
                .flex_shrink_0()
                .child(div().flex_1().child(tr("texture-title")))
                .child(
                    IconButton::new(
                        "material-panel-enabled",
                        if session.enabled { "󰔡" } else { "󰔢" },
                        tr("material-enabled"),
                    )
                    .switch(session.enabled)
                    .build(
                        IconButtonStyle::Quiet,
                        IconButtonDensity::Compact,
                        cx,
                        move |this, _, cx| {
                            this.material_intent(MaterialIntent::SetEnabled(!session.enabled), cx);
                        },
                    ),
                ),
        )
        .child(choices)
        .child(body);
    let popup = anchor
        .get()
        .filter(|_| session.enabled && view.shell.material_choices.is_open())
        .map(|bounds| {
            ChoicePopup::new("material-preset-popup", "material-preset-scroll", bounds).render(
                view,
                |this| &this.shell.material_choices,
                crate::gui::WindowView::choose_material_preset,
                |this, _, _, _| {
                    this.shell.material_choices.close();
                    true
                },
                window,
                cx,
            )
        });
    Dialog::backdrop("texture-controls-modal")
        .bg(transparent_black())
        .justify_end()
        .pt(rems(0.5))
        .pl(rems(0.5))
        .pr(right_inset)
        .pb(bottom_inset)
        .items_end()
        .map(|surface| match scope {
            Some(scope) => scope.surface(surface),
            None => surface,
        })
        .on_click(cx.listener(|this, _, _, cx| {
            this.close_material_controls(cx);
            cx.stop_propagation();
        }))
        .children(footer_bounds.map(|_| content))
        .children(popup)
        .into_any_element()
}
