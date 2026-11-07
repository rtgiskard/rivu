//! Water materials share the caller's artwork; motion never creates image sources.
use std::{f32::consts::TAU, time::Duration};

use gpui::{prelude::*, *};
use rivu_format::palette::Palette;

fn water_layer() -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
}

pub(super) fn ripple(theme: &'static Palette, _image: Option<Img>, motion: bool) -> Div {
    let light = theme.bg > 0x808080;
    water_layer()
        .bg(rgb(theme.bg))
        .child(
            div()
                .absolute()
                .top(relative(0.38))
                .left_0()
                .w_full()
                .h(relative(0.62))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(
                        rgb(theme.accent).alpha(if light { 0.055 } else { 0.045 }),
                        0.,
                    ),
                    linear_color_stop(rgb(theme.control).alpha(0.18), 1.),
                )),
        )
        .child(ripple_field(theme, motion, false))
        .child(water_layer().bg(linear_gradient(
            90.,
            linear_color_stop(rgb(theme.bg).alpha(0.88), 0.),
            linear_color_stop(rgb(theme.bg).alpha(0.), 1.),
        )))
}

fn ripple_field(theme: &'static Palette, motion: bool, panel: bool) -> Div {
    let field = water_layer();
    if motion {
        water_layer().child(field.with_animation(
            if panel {
                "water-ripple-panel"
            } else {
                "water-ripple-field"
            },
            Animation::new(Duration::from_secs(9)).repeat_synced(),
            move |field, phase| field.child(ripple_canvas(theme, phase, panel)),
        ))
    } else {
        field.child(ripple_canvas(theme, 0.19, panel))
    }
}

fn ripple_canvas(theme: &'static Palette, phase: f32, panel: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            // Six wavefronts, not capsules: each is four elliptical cubic arcs.
            // The two sources have fixed phase offsets; radius grows while both
            // ends of the lifetime fade to zero, hiding the wrapping phase.
            // Preparation stays bounded at six contours plus six crest arcs.
            for index in 0..6 {
                let primary = index < 4;
                let offset = if primary {
                    index as f32 / 4.
                } else {
                    (index - 4) as f32 / 2. + 0.13
                };
                let age = (phase + offset).fract();
                let envelope = (std::f32::consts::PI * age).sin().powf(0.8);
                let (x, y, reach) = if primary {
                    (
                        if panel { 0.85 } else { 0.72 },
                        if panel { 0.82 } else { 0.65 },
                        0.64,
                    )
                } else {
                    (0.28, if panel { 0.96 } else { 0.83 }, 0.38)
                };
                let center = point(
                    bounds.left() + bounds.size.width * x,
                    bounds.top() + bounds.size.height * y,
                );
                let radius_x = bounds.size.width * (0.018 + age * reach);
                let radius_y = bounds.size.height * (0.006 + age * reach * 0.29);
                let alpha = envelope * if panel { 0.34 } else { 0.25 };
                paint_wave(window, center, radius_x, radius_y, theme, alpha);
            }
        },
    )
    .absolute()
    .size_full()
}

fn paint_wave(
    window: &mut Window,
    center: Point<Pixels>,
    rx: Pixels,
    ry: Pixels,
    theme: &'static Palette,
    alpha: f32,
) {
    // A true ellipse maintains curvature even at wide, shallow water angles.
    const K: f32 = 0.552_284_8;
    let Point { x, y } = center;
    let mut contour = PathBuilder::stroke(px(1.1));
    contour.move_to(point(x + rx, y));
    contour.cubic_bezier_to(
        point(x, y + ry),
        point(x + rx, y + ry * K),
        point(x + rx * K, y + ry),
    );
    contour.cubic_bezier_to(
        point(x - rx, y),
        point(x - rx * K, y + ry),
        point(x - rx, y + ry * K),
    );
    contour.cubic_bezier_to(
        point(x, y - ry),
        point(x - rx, y - ry * K),
        point(x - rx * K, y - ry),
    );
    contour.cubic_bezier_to(
        point(x + rx, y),
        point(x + rx * K, y - ry),
        point(x + rx, y - ry * K),
    );
    contour.close();
    if let Ok(path) = contour.build() {
        window.paint_path(path, rgb(theme.accent).alpha(alpha));
    }
    // Only the near-facing quarter catches light; no concentric neon target.
    let mut crest = PathBuilder::stroke(px(0.8));
    crest.move_to(point(x + rx, y + px(1.4)));
    crest.cubic_bezier_to(
        point(x, y + ry + px(1.4)),
        point(x + rx, y + ry * K + px(1.4)),
        point(x + rx * K, y + ry + px(1.4)),
    );
    if let Ok(path) = crest.build() {
        window.paint_path(path, rgb(theme.text).alpha(alpha * 0.38));
    }
}

pub(super) fn mirror(theme: &'static Palette, images: Option<[Img; 16]>, motion: bool) -> Div {
    let light = theme.bg > 0x808080;
    let mut water = div()
        .absolute()
        .top(relative(0.42))
        .left_0()
        .w_full()
        .h(relative(0.58))
        .overflow_hidden()
        .bg(linear_gradient(
            180.,
            linear_color_stop(
                rgb(theme.accent).alpha(if light { 0.08 } else { 0.065 }),
                0.,
            ),
            linear_color_stop(rgb(theme.bg), 1.),
        ));

    // Every strip contains the SAME full-field Cover rectangle: its height is
    // 16 strip heights and its top is -index strip heights. Consequently every
    // flip has the same full-field pivot, not a separate flip inside each band.
    // Clipping remains in screen coordinates after the GPU transformation.
    // Width 108%, centered at -4%, covers the maximum 2.6% horizontal excursion.
    // With motion off all strips align into one genuinely flipped photograph.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    if let Some(images) = images {
        for (index, image) in images.into_iter().enumerate() {
            let image = image
                .absolute()
                .top(relative(-(index as f32)))
                .left(relative(-0.04))
                .w(relative(1.08))
                .h(relative(16.))
                .object_fit(ObjectFit::Cover)
                .with_transformation(Transformation::scale(size(1., -1.)))
                .opacity(if light { 0.67 } else { 0.74 });
            let band = div()
                .absolute()
                .left_0()
                .top(relative(index as f32 / 16.))
                .w_full()
                .h(relative(1. / 16.))
                .overflow_hidden();
            water = water.child(if motion {
                band.child(image.with_animation(
                    ("water-mirror-band", index),
                    Animation::new(Duration::from_secs(6)).repeat_synced(),
                    move |image, phase| {
                        let depth = (index as f32 + 0.5) / 16.;
                        let angle = TAU * (phase - depth * 1.35);
                        // Neighboring strips sample one travelling wave, with
                        // a weaker harmonic and increasing displacement below.
                        // Moving the image itself shears real artwork detail;
                        // the fixed clip/hitbox never moves with the photograph.
                        let displacement = (0.002 + 0.020 * depth) * angle.sin()
                            + 0.004 * depth * (angle * 2. + 0.7).sin();
                        image.left(relative(-0.04 + displacement))
                    },
                ))
            } else {
                band.child(image)
            });
        }
    }
    // Native Metal rejects image transformations. Preserve semantic water and
    // horizon glints there, never an unflipped or separately decoded substitute.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let _ = images;

    water_layer()
        .bg(rgb(theme.bg))
        .child(water.child(water_layer().bg(linear_gradient(
            180.,
            linear_color_stop(rgb(theme.bg).alpha(0.02), 0.),
            linear_color_stop(rgb(theme.bg).alpha(0.86), 1.),
        ))))
        .child(
            div()
                .absolute()
                .top(relative(0.42))
                .left_0()
                .w_full()
                .h(px(1.))
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(rgb(theme.accent).alpha(if light { 0.22 } else { 0.19 }), 0.),
                    linear_color_stop(rgb(theme.accent).alpha(0.), 1.),
                )),
        )
        .child(mirror_glints(theme, motion, false))
}

pub(super) fn ripple_panel(theme: &'static Palette, playback: bool, motion: bool) -> Div {
    if playback {
        ripple_field(theme, motion, true)
    } else {
        water_layer()
    }
}

pub(super) fn mirror_panel(theme: &'static Palette, playback: bool, motion: bool) -> Div {
    if !playback {
        return water_layer();
    }
    let light = theme.bg > 0x808080;
    water_layer()
        .child(
            div()
                .absolute()
                .top(relative(0.62))
                .left_0()
                .w_full()
                .h(relative(0.38))
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(
                        rgb(theme.accent).alpha(if light { 0.045 } else { 0.04 }),
                        0.,
                    ),
                    linear_color_stop(rgb(theme.panel).alpha(0.), 1.),
                )),
        )
        .child(mirror_glints(theme, motion, true))
}

fn mirror_glints(theme: &'static Palette, motion: bool, panel: bool) -> Div {
    let mut layer = water_layer();
    for (index, (left, depth, width)) in [
        (0.12, 0.03, 0.48),
        (0.56, 0.10, 0.30),
        (0.32, 0.21, 0.19),
        (0.68, 0.37, 0.24),
    ]
    .into_iter()
    .enumerate()
    {
        let horizon = if panel { 0.62 } else { 0.42 };
        let glint = div()
            .absolute()
            .left(relative(left))
            .top(relative(horizon + depth * (1. - horizon)))
            .w(relative(width))
            .h(px(1.))
            .bg(linear_gradient(
                90.,
                linear_color_stop(rgb(theme.text).alpha(0.), 0.),
                linear_color_stop(rgb(theme.text).alpha(if panel { 0.18 } else { 0.09 }), 1.),
            ));
        layer = if motion {
            layer.child(glint.with_animation(
                (
                    if panel {
                        "water-mirror-panel-glint"
                    } else {
                        "water-mirror-glint"
                    },
                    index,
                ),
                Animation::new(Duration::from_secs(6)).repeat_synced(),
                move |glint, phase| {
                    let wave = (TAU * (phase - depth * 1.35)).sin();
                    glint
                        .left(relative(left + wave * 0.018))
                        .w(relative(width * (0.88 + 0.12 * wave)))
                        .opacity(0.68 + 0.32 * wave)
                },
            ))
        } else {
            layer.child(glint)
        };
    }
    layer
}
