//! Session-only material controls. State and capabilities belong to material;
//! the shell owns modal attachment and intents reconcile through window coordination.
use super::material::{
    Material, MaterialGroup, MaterialIntent, MaterialSession, Parameter, TextureSettings,
};
use gpui::{prelude::*, *};
use rivu_ui::{Dialog, FocusScope, button_style, caption, column, disabled_control, row};

impl Parameter {
    fn section(self) -> usize {
        match self {
            Self::Alpha | Self::BackdropBlur => 0,
            Self::Feather | Self::CoverBlur | Self::Transition => 1,
            Self::Highlight | Self::Wave | Self::Grain | Self::Steps => 2,
            Self::Speed => 3,
            Self::EdgeFade => 4,
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

fn preset_choice(
    candidate: Material,
    selected: Material,
    tr: &impl Fn(&str) -> SharedString,
    cx: &mut Context<crate::gui::WindowView>,
) -> Stateful<Div> {
    let theme = rivu_ui::theme::palette(cx);
    rivu_ui::active_control(
        theme,
        button_style(
            theme,
            ("material-preset", candidate as usize),
            tr(candidate.label()),
        )
        .w_full()
        .justify_start()
        .aria_toggled(if candidate == selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .on_click(cx.listener(move |this, _, _, cx| {
            this.material_intent(MaterialIntent::Select(candidate), cx);
            cx.stop_propagation();
        })),
        candidate == selected,
    )
}

/// Attach after the workspace; the dialog exists only in the window that opened it.
pub(in crate::gui) fn render(
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
    let wide = window.viewport_size().width >= window.rem_size() * 52.;
    let mut choices = column()
        .gap_2()
        .min_w_0()
        .flex_shrink_0()
        .when(wide, |list| list.w(rems(14.)))
        .when(!wide, |list| list.w_full())
        .child(preset_choice(Material::Original, material, &tr, cx));
    for group in MaterialGroup::ALL {
        let experimental = group == MaterialGroup::Experimental;
        let selected_in_group = material.group() == Some(group);
        let mut section = column().gap_1().flex_shrink_0();
        if experimental {
            section = section.child(rivu_ui::active_control(
                theme,
                button_style(theme, "material-experimental", tr(group.label()))
                    .w_full()
                    .justify_start()
                    .aria_expanded(experimental_expanded)
                    .child(
                        div()
                            .ml_auto()
                            .child(if experimental_expanded { "−" } else { "+" }),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_experimental_materials(cx);
                        cx.stop_propagation();
                    })),
                selected_in_group,
            ));
        } else {
            section = section.child(caption(theme, tr(group.label())));
        }
        if !experimental || experimental_expanded {
            for candidate in Material::ALL {
                if candidate.group() == Some(group) {
                    section = section.child(preset_choice(candidate, material, &tr, cx));
                }
            }
        }
        choices = choices.child(section);
    }
    let status = if session.modified() {
        tr("texture-modified")
    } else {
        tr("texture-defaults")
    };
    let mut parameters = column().flex_1().min_w_0().gap_3().child(
        column()
            .gap_1()
            .child(div().child(tr(material.label())))
            .child(caption(theme, status)),
    );
    if !session.enabled {
        parameters = parameters.child(caption(theme, tr("material-disabled-info")));
    }
    let sections = [
        "material-section-surface",
        "material-section-cover",
        "material-section-optics",
        "material-section-motion",
        "material-section-list",
    ];
    for (section_index, label) in sections.into_iter().enumerate() {
        let mut controls_in_section = material
            .spec()
            .controls()
            .enumerate()
            .filter(|(_, parameter)| parameter.section() == section_index)
            .peekable();
        if controls_in_section.peek().is_none() {
            continue;
        }
        let mut section = column()
            .gap_2()
            .flex_shrink_0()
            .child(caption(theme, tr(label)));
        for (index, parameter) in controls_in_section {
            let value = parameter.value(settings);
            let mut controls = row().gap_1().flex_shrink_0();
            for increase in [false, true] {
                let enabled = parameter.next(value, increase) != value;
                let control = button_style(
                    theme,
                    ("texture-parameter", index * 2 + usize::from(increase)),
                    if increase { "+" } else { "−" },
                )
                .min_w_8()
                .aria_label(format!(
                    "{} {}",
                    tr(if increase {
                        "texture-increase"
                    } else {
                        "texture-decrease"
                    }),
                    tr(parameter.label(material))
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if enabled {
                        this.material_intent(
                            MaterialIntent::Adjust {
                                material,
                                parameter,
                                increase,
                            },
                            cx,
                        );
                    }
                    cx.stop_propagation();
                }));
                controls = controls.child(disabled_control(theme, control, !enabled));
            }
            section = section.child(
                row()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(tr(parameter.label(material))),
                    )
                    .child(
                        div()
                            .w(rems(4.5))
                            .flex_shrink_0()
                            .text_right()
                            .child(parameter.display(material, settings, &tr)),
                    )
                    .child(controls),
            );
        }
        parameters = parameters.child(section);
    }
    if material.spec().controls().next().is_none() {
        parameters = parameters.child(caption(theme, tr("material-fixed-info")));
    }
    let body = div()
        .id("material-controls-body")
        .flex()
        .when(wide, |body| body.flex_row())
        .when(!wide, |body| body.flex_col())
        .items_start()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .gap_4()
        .child(choices)
        .child(parameters.when(!wide, |parameters| parameters.w_full().flex_shrink_0()));
    let content = Dialog::card(theme)
        .id("texture-controls-card")
        .w(rems(48.))
        .max_w(relative(0.95))
        .max_h(window.viewport_size().height * 0.9)
        .overflow_hidden()
        .p_3()
        .gap_3()
        .role(Role::Dialog)
        .aria_label(tr("texture-title"))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(
            column()
                .gap_1()
                .flex_shrink_0()
                .child(div().child(tr("texture-title")))
                .child(caption(theme, tr("texture-live-info"))),
        )
        .child(body)
        .child(
            row()
                .gap_2()
                .flex_shrink_0()
                .flex_wrap()
                .child(rivu_ui::active_control(
                    theme,
                    button_style(theme, "material-panel-enabled", tr("material-enabled"))
                        .aria_toggled(if session.enabled {
                            Toggled::True
                        } else {
                            Toggled::False
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.material_intent(MaterialIntent::SetEnabled(!session.enabled), cx);
                            cx.stop_propagation();
                        })),
                    session.enabled,
                ))
                .child(div().flex_1())
                .child(
                    button_style(theme, "texture-reset", tr("texture-reset")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.material_intent(MaterialIntent::Reset(material), cx);
                            cx.stop_propagation();
                        }),
                    ),
                )
                .child(
                    button_style(theme, "texture-close", tr("texture-close")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.close_material_controls(cx);
                            cx.stop_propagation();
                        }),
                    ),
                ),
        );
    Dialog::backdrop("texture-controls-modal")
        .bg(transparent_black())
        .justify_end()
        .p_2()
        .map(|surface| match scope {
            Some(scope) => scope.surface(surface),
            None => surface,
        })
        .on_click(cx.listener(|this, _, _, cx| {
            this.close_material_controls(cx);
            cx.stop_propagation();
        }))
        .child(content)
        .into_any_element()
}
