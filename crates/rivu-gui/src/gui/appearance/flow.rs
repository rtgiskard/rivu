//! Faceted glass reflections and local matte illumination, painted beneath panel content.
use gpui::{prelude::*, *};
use rivu_format::palette::Palette;
use std::time::Duration;

pub(super) fn spring(theme: &'static Palette, _image: Option<Img>, motion: bool) -> Div {
    let light = theme.bg > 0x808080;
    let rim = rgb(theme.text).alpha(if light { 0.20 } else { 0.22 });
    // A faceted pane, not an atmospheric wash: its seam crosses the workspace.
    let layer = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .bg(rgb(theme.bg))
        .child(div().absolute().size_full().bg(linear_gradient(
            125.,
            linear_color_stop(rgb(theme.control), 0.47),
            linear_color_stop(
                rgb(theme.accent).alpha(if light { 0.24 } else { 0.20 }),
                0.49,
            ),
        )))
        .child(
            div()
                .absolute()
                .top_0()
                .left(relative(0.16))
                .w(relative(0.32))
                .h_full()
                .border_l_1()
                .border_r_1()
                .border_color(rim)
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.panel).alpha(0.04), 0.),
                    linear_color_stop(rgb(theme.text).alpha(if light { 0.08 } else { 0.055 }), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left(relative(0.48))
                .w(px(3.))
                .h_full()
                .bg(rgb(theme.text).alpha(if light { 0.12 } else { 0.16 })),
        )
        .child(
            div()
                .absolute()
                .bottom(relative(0.12))
                .left_0()
                .w_full()
                .h(px(1.))
                .bg(rim),
        );
    if motion {
        layer.child(div().absolute().size_full().with_animation(
            "spring-root-reflection",
            Animation::new(Duration::from_secs(18)).repeat_synced(),
            move |pane, phase| pane.child(spring_reflection(theme, reflected_position(phase))),
        ))
    } else {
        layer.child(spring_reflection(theme, 0.38))
    }
}

pub(super) fn glint(theme: &'static Palette, _image: Option<Img>, motion: bool) -> Div {
    // Opaque negative space is the material. Light is local to playback, not glass.
    let layer = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .bg(rgb(theme.bg))
        .child(
            div()
                .absolute()
                .left(relative(0.045))
                .bottom_0()
                .w(px(1.))
                .h(relative(0.34))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(rgb(theme.accent).alpha(0.), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.65), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .left(relative(0.045))
                .bottom_0()
                .w(relative(0.52))
                .h(px(2.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.accent).alpha(0.55), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.), 1.),
                )),
        );
    if motion {
        layer.child(div().absolute().size_full().with_animation(
            "glint-root-ribbon",
            Animation::new(Duration::from_secs(14)).repeat_synced(),
            move |pane, phase| pane.child(glint_ribbon(theme, reflected_position(phase))),
        ))
    } else {
        layer.child(glint_ribbon(theme, 0.38))
    }
}

pub(super) fn spring_panel(theme: &'static Palette, playback: bool, motion: bool) -> Div {
    let light = theme.bg > 0x808080;
    let edge = rgb(theme.text).alpha(if light { 0.25 } else { 0.35 });
    let layer = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .w(relative(if playback { 0.24 } else { 0.12 }))
                .h_full()
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.text).alpha(0.), 0.),
                    linear_color_stop(rgb(theme.text).alpha(if light { 0.08 } else { 0.045 }), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h(px(2.))
                .bg(edge),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .w(px(2.))
                .h_full()
                .bg(rgb(theme.accent).alpha(if playback { 0.48 } else { 0.18 })),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .w_full()
                .h(px(2.))
                .bg(rgb(theme.bg).alpha(0.40)),
        );
    if motion {
        layer.child(div().absolute().size_full().with_animation(
            "spring-panel-reflection",
            Animation::new(Duration::from_secs(18)).repeat_synced(),
            move |pane, phase| {
                pane.child(spring_bevel_reflection(
                    theme,
                    playback,
                    reflected_position(phase),
                ))
            },
        ))
    } else {
        layer.child(spring_bevel_reflection(theme, playback, 0.38))
    }
}

pub(super) fn glint_panel(theme: &'static Palette, playback: bool, motion: bool) -> Div {
    let layer = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
        .bg(rgb(if playback { theme.panel } else { theme.bg }));
    // Reading panes remain opaque and still; only playback carries concentrated light.
    if !playback {
        return layer;
    }
    if motion {
        layer.child(div().absolute().size_full().with_animation(
            "glint-playback-illumination",
            Animation::new(Duration::from_secs(14)).repeat_synced(),
            move |pane, phase| pane.child(glint_playback_light(theme, reflected_position(phase))),
        ))
    } else {
        layer.child(glint_playback_light(theme, 0.38))
    }
}

// A continuous out-and-back reflection: the loop has neither a position nor velocity jump.
fn reflected_position(phase: f32) -> f32 {
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

fn spring_reflection(theme: &'static Palette, position: f32) -> Div {
    let light = theme.bg > 0x808080;
    let specular = rgb(if light { 0xffffff } else { theme.text });
    div()
        .absolute()
        .size_full()
        .child(
            div()
                .absolute()
                .top_0()
                .left(relative(0.16))
                .w(relative(0.32))
                .h_full()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left(relative(0.04 + 0.78 * position))
                        .w(relative(0.14))
                        .h_full()
                        .border_r_1()
                        .border_color(specular.alpha(if light { 0.22 } else { 0.32 }))
                        .bg(linear_gradient(
                            90.,
                            linear_color_stop(specular.alpha(0.), 0.),
                            linear_color_stop(specular.alpha(if light { 0.10 } else { 0.08 }), 1.),
                        )),
                ),
        )
        .child(
            div()
                .absolute()
                .left(relative(0.48))
                .top(relative(0.04 + 0.66 * position))
                .w(px(3.))
                .h(relative(0.25))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(specular.alpha(0.), 0.),
                    linear_color_stop(specular.alpha(if light { 0.36 } else { 0.52 }), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom(relative(0.12))
                .left(relative(0.10 + 0.62 * position))
                .w(relative(0.22))
                .h(px(1.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(specular.alpha(0.), 0.),
                    linear_color_stop(specular.alpha(if light { 0.42 } else { 0.55 }), 1.),
                )),
        )
}

fn spring_bevel_reflection(theme: &'static Palette, playback: bool, position: f32) -> Div {
    let light = theme.bg > 0x808080;
    let specular = rgb(if light { 0xffffff } else { theme.text });
    let strength = if playback { 1.0 } else { 0.65 };
    div()
        .absolute()
        .size_full()
        .child(
            div()
                .absolute()
                .top_0()
                .left(relative(0.20 + 0.50 * position))
                .w(relative(0.18))
                .h_full()
                .border_r_1()
                .border_color(specular.alpha(strength * if light { 0.07 } else { 0.09 }))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(specular.alpha(0.), 0.),
                    linear_color_stop(specular.alpha(strength * 0.035), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left(relative(0.04 + 0.60 * position))
                .w(relative(0.30))
                .h(px(2.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(specular.alpha(0.), 0.),
                    linear_color_stop(
                        specular.alpha(strength * if light { 0.40 } else { 0.60 }),
                        1.,
                    ),
                )),
        )
        .child(
            div()
                .absolute()
                .right_0()
                .top(relative(0.06 + 0.60 * position))
                .w(px(2.))
                .h(relative(0.28))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(specular.alpha(0.), 0.),
                    linear_color_stop(
                        specular.alpha(strength * if light { 0.40 } else { 0.65 }),
                        1.,
                    ),
                )),
        )
}

fn glint_ribbon(theme: &'static Palette, position: f32) -> Div {
    let light = theme.bg > 0x808080;
    div()
        .absolute()
        .left(relative(0.045 + 0.16 * position))
        .bottom_0()
        .w(relative(0.24))
        .h(px(28.))
        .bg(linear_gradient(
            180.,
            linear_color_stop(rgb(theme.accent).alpha(0.), 0.),
            linear_color_stop(
                rgb(theme.accent)
                    .alpha((if light { 0.055 } else { 0.10 }) * (0.6 + 0.4 * position)),
                1.,
            ),
        ))
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .w_full()
                .h(px(2.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.accent).alpha(0.), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.25 + 0.20 * position), 1.),
                )),
        )
}

fn glint_playback_light(theme: &'static Palette, position: f32) -> Div {
    let light = theme.bg > 0x808080;
    let intensity = 0.72 + 0.28 * position;
    div()
        .absolute()
        .size_full()
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(relative(0.44 + 0.16 * position))
                .h_full()
                .bg(linear_gradient(
                    100.,
                    linear_color_stop(rgb(theme.accent).alpha(0.24 * intensity), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top(relative(0.08 + 0.46 * position))
                .w(relative(0.38))
                .h(relative(0.18))
                .bg(linear_gradient(
                    110.,
                    linear_color_stop(rgb(theme.accent).alpha(if light { 0.10 } else { 0.14 }), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.), 1.),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(px(2.))
                .h_full()
                .bg(rgb(theme.accent).alpha(0.60 + 0.25 * position)),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(relative(0.36 + 0.18 * position))
                .h(px(1.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.text).alpha(0.48 * intensity), 0.),
                    linear_color_stop(rgb(theme.text).alpha(0.), 1.),
                )),
        )
}
