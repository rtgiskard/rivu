//! Session material layers. Every decoration precedes semantic content.
//! Artwork is shared; T through Z use a procedural canvas and never sample a cover.
use crate::gui::shell::panels::PanelKind;
use std::{f32::consts::TAU, time::Duration};

use gpui::{prelude::*, *};
use rivu_format::palette::Palette;

use super::material::{MaterialAppearance, MaterialParametersAppearance};
use rivu_format::config::{MaterialParameters, MaterialPreset};

/// One accepted artwork transition, consumed as a coherent set of source views.
pub(in crate::gui) struct ArtworkBlend {
    pub previous: Option<Img>,
    pub current: Option<Img>,
    pub weight: f32,
}

impl ArtworkBlend {
    fn opacities(&self, opacity: f32, transition: bool) -> (f32, f32) {
        let weight = if transition && self.previous.is_some() {
            if self.current.is_some() {
                self.weight.clamp(0., 1.)
            } else {
                0.
            }
        } else {
            1.
        };
        let current_alpha = if self.current.is_some() {
            opacity * weight
        } else {
            0.
        };
        // Source-over compensation keeps the backdrop contribution constant.
        // For feathered covers, masks still compose by source-over coverage.
        let previous_alpha = if transition && self.previous.is_some() {
            opacity * (1. - weight) / (1. - current_alpha).max(0.001)
        } else {
            0.
        };
        (previous_alpha, current_alpha)
    }
}

fn layer() -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .overflow_hidden()
}

/// The root is a bounded scene, never an offscreen image or a second artwork cache.
pub(in crate::gui) fn backdrop(
    material: MaterialPreset,
    settings: MaterialParameters,
    theme: &'static Palette,
    images: ArtworkBlend,
    motion: bool,
) -> Div {
    if material.is_procedural_water() {
        return cg_water(material, settings, theme, motion);
    }
    let light = theme.bg > 0x808080;
    let mut root = layer().bg(linear_gradient(
        135.,
        linear_color_stop(rgb(theme.bg), 0.),
        linear_color_stop(rgb(theme.control), 1.),
    ));
    let opacity = match material {
        MaterialPreset::Gradient => 0.16,
        MaterialPreset::Feather => 0.72,
        MaterialPreset::Sheen | MaterialPreset::EdgeFade => 0.25,
        MaterialPreset::Aura => {
            if light {
                0.46
            } else {
                0.56
            }
        }
        MaterialPreset::Water => 0.72,
        MaterialPreset::Grain => 0.12,
        MaterialPreset::Dissolve => 0.56,
        MaterialPreset::Frost => 0.42,
        MaterialPreset::Storybook => 0.12,
        _ => 0.,
    };
    let art = artwork(material, settings, images, opacity, motion, false);
    root = if material == MaterialPreset::Water {
        root.child(
            div()
                .absolute()
                .top(relative(0.32))
                .left_0()
                .w_full()
                .h(relative(0.68))
                .overflow_hidden()
                .child(art),
        )
    } else {
        root.child(art)
    };
    if matches!(material, MaterialPreset::Frost | MaterialPreset::Storybook) {
        root = root.child(scene(material, settings, theme, motion, false));
    }
    if settings.grain > 0. {
        root = root.child(paper(theme, settings.grain, false));
    }
    // Readability is local to panel tints; do not wash away the root effects here.
    root
}

/// Apply the backend backdrop blur to the panel BEFORE calling this helper for R.
/// Apply M's group_edge_fade to the actual list content, never this decoration.
pub(in crate::gui) fn panel(
    material: MaterialPreset,
    settings: MaterialParameters,
    panel: Stateful<Div>,
    kind: &str,
    theme: &'static Palette,
    images: ArtworkBlend,
    motion: bool,
) -> Stateful<Div> {
    let playback = matches!(
        PanelKind::from_key(kind),
        Some(PanelKind::Transport | PanelKind::Artwork)
    );
    let light = theme.bg > 0x808080;
    let alpha = settings.panel_alpha.clamp(0., 1.);
    let mut panel = panel
        .bg(transparent_black())
        .border_color(rgb(theme.border).alpha(if light { 0.42 } else { 0.55 }));
    // T through Z paint water only at the root. Panel tint never transforms covers or text.
    if material.is_procedural_water() {
        return panel.child(layer().bg(rgb(theme.panel).alpha(alpha)));
    }
    // R must sample the arbitrary root scene, not an image painted inside its panel.
    if material != MaterialPreset::Frost && playback {
        panel = panel.child(artwork(
            material,
            settings,
            images,
            if material == MaterialPreset::Grain {
                0.20
            } else {
                0.58
            },
            motion,
            true,
        ));
    }
    let tint_angle = if material == MaterialPreset::Gradient {
        120.
    } else {
        155.
    };
    panel = panel.child(layer().bg(linear_gradient(
        tint_angle,
        linear_color_stop(rgb(theme.control).alpha(alpha * 0.72), 0.),
        linear_color_stop(rgb(theme.panel).alpha(alpha), 1.),
    )));
    if material == MaterialPreset::Storybook {
        panel = panel.child(scene(material, settings, theme, motion && playback, true));
    }
    if settings.grain > 0. {
        panel = panel.child(paper(theme, settings.grain, true));
    }
    if settings.highlight > 0. {
        let sheen = layer();
        panel = if motion && settings.speed > 0. {
            panel.child(sheen.with_animation(
                "texture-local-sheen",
                Animation::new(cycle(13., settings.speed)).repeat_synced(),
                move |pane, phase| pane.child(sheen_band(theme, settings.highlight, phase)),
            ))
        } else {
            panel.child(sheen.child(sheen_band(theme, settings.highlight, 0.20)))
        };
    }
    panel.child(
        div()
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(1.))
            .bg(rgb(theme.text).alpha(if light { 0.12 } else { 0.10 })),
    )
}

fn cg_water(
    material: MaterialPreset,
    settings: MaterialParameters,
    theme: &'static Palette,
    motion: bool,
) -> Div {
    if settings.cg_water_motion(motion) {
        layer().child(layer().with_animation(
            material.label(),
            Animation::new(cycle(24., settings.speed)).repeat_synced(),
            move |pane, phase| pane.child(cg_water_canvas(material, settings, theme, phase)),
        ))
    } else {
        layer().child(cg_water_canvas(material, settings, theme, 0.))
    }
}

fn cg_water_canvas(
    material: MaterialPreset,
    settings: MaterialParameters,
    theme: &'static Palette,
    phase: f32,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_cg_water(
                bounds,
                settings.cg_water_params(material, phase, theme.bg > 0x808080),
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn cycle(seconds: f32, speed: f32) -> Duration {
    Duration::from_secs_f32(seconds / speed.clamp(0.05, 3.))
}

/// All fields compose on one source sample; material selection never selects a shader mode.
fn effects(settings: MaterialParameters, phase: f32) -> ImageEffects {
    let feather = settings.feather.clamp(0., 1.);
    ImageEffects {
        linear_fade_start: if feather > 0. {
            1. - feather * 0.80
        } else {
            0.
        },
        linear_fade_end: if feather > 0. { 1. } else { 0. },
        linear_fade_angle: std::f32::consts::FRAC_PI_2,
        radial_fade_start: if feather > 0. {
            1. - feather * 0.65
        } else {
            0.
        },
        radial_fade_end: if feather > 0. { 1. } else { 0. },
        blur_radius: settings.cover_blur.clamp(0., 24.),
        wave_amplitude: settings.wave.clamp(0., 16.),
        wave_frequency: 3.5,
        wave_phase: phase * TAU,
        grain: settings.grain.clamp(0., 1.),
        grain_scale: 1.25,
        posterize_levels: if settings.color_steps >= 2. {
            settings.color_steps
        } else {
            0.
        },
    }
}

fn image_layer(
    image: Img,
    material: MaterialPreset,
    settings: MaterialParameters,
    motion: bool,
    animation_id: &'static str,
) -> Div {
    let mut image = image
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .object_fit(ObjectFit::Cover)
        .with_effects(effects(settings, 0.));
    if material == MaterialPreset::Water {
        image = image.with_transformation(Transformation::scale(size(1., -1.)));
    }
    if motion && settings.speed > 0. && settings.wave > 0. {
        layer().child(image.with_animation(
            animation_id,
            Animation::new(cycle(10., settings.speed)).repeat_synced(),
            move |image, phase| image.with_effects(effects(settings, phase)),
        ))
    } else {
        layer().child(image)
    }
}

fn artwork(
    material: MaterialPreset,
    settings: MaterialParameters,
    images: ArtworkBlend,
    opacity: f32,
    motion: bool,
    panel: bool,
) -> Div {
    let (previous_alpha, current_alpha) =
        images.opacities(opacity, settings.transition_seconds > 0.);
    let ArtworkBlend {
        previous, current, ..
    } = images;
    let mut result = layer();
    if previous_alpha > 0.
        && let Some(previous) = previous
    {
        result = result.child(
            image_layer(
                previous,
                material,
                settings,
                motion,
                if panel {
                    "texture-panel-previous-uv"
                } else {
                    "texture-root-previous-uv"
                },
            )
            .opacity(previous_alpha),
        );
    }
    if let Some(current) = current {
        result = result.child(
            image_layer(
                current,
                material,
                settings,
                motion,
                if panel {
                    "texture-panel-current-uv"
                } else {
                    "texture-root-current-uv"
                },
            )
            .opacity(current_alpha),
        );
    }
    result
}

fn sheen_band(theme: &'static Palette, highlight: f32, phase: f32) -> Div {
    let position = 0.5 - 0.5 * (phase * TAU).cos();
    let white = rgb(if theme.bg > 0x808080 {
        0xffffff
    } else {
        theme.text
    });
    div()
        .absolute()
        .top_0()
        .left(relative(-0.18 + 1.12 * position))
        .w(relative(0.22))
        .h_full()
        .bg(linear_gradient(
            105.,
            linear_color_stop(white.alpha(0.), 0.),
            linear_color_stop(white.alpha(highlight.clamp(0., 1.) * 0.22), 1.),
        ))
        .child(
            div()
                .absolute()
                .right_0()
                .top_0()
                .w(px(1.))
                .h_full()
                .bg(white.alpha(highlight.clamp(0., 1.) * 0.25)),
        )
}

const PAPER_FIBERS: [[f32; 4]; 320] = {
    let mut samples = [[0.; 4]; 320];
    let mut i = 0;
    while i < samples.len() {
        let index = i as u32;
        samples[i] = [
            hash(index * 2 + 17),
            hash(index * 2 + 901),
            hash(index + 41),
            hash(index + 73),
        ];
        i += 1;
    }
    samples
};

/// A deterministic bounded fiber field; no time seed, image generation, or cache.
fn paper(theme: &'static Palette, strength: f32, panel: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let mut fibers = PathBuilder::stroke(px(0.65));
            let count = if panel { 112 } else { 320 };
            for &[x, y, length, slope] in &PAPER_FIBERS[..count] {
                let p = point(
                    bounds.left() + bounds.size.width * x,
                    bounds.top() + bounds.size.height * y,
                );
                fibers.move_to(p);
                fibers.line_to(point(
                    p.x + px(0.7 + length * 2.1),
                    p.y + px((slope - 0.5) * 1.3),
                ));
            }
            if let Ok(path) = fibers.build() {
                window.paint_path(path, rgb(theme.text).alpha(strength.clamp(0., 1.) * 0.28));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

const fn hash(index: u32) -> f32 {
    let mut value = index.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    value = ((value >> ((value >> 28) + 4)) ^ value).wrapping_mul(277_803_737);
    ((value >> 22) ^ value) as f32 / u32::MAX as f32
}

fn scene(
    material: MaterialPreset,
    settings: MaterialParameters,
    theme: &'static Palette,
    motion: bool,
    panel: bool,
) -> Div {
    if motion && settings.speed > 0. {
        layer().child(layer().with_animation(
            if panel {
                "storybook-local-canopy"
            } else {
                "texture-root-scene"
            },
            Animation::new(cycle(26., settings.speed)).repeat_synced(),
            move |pane, phase| pane.child(scene_canvas(material, theme, phase, panel)),
        ))
    } else {
        layer().child(scene_canvas(material, theme, 0., panel))
    }
}

fn scene_canvas(
    material: MaterialPreset,
    theme: &'static Palette,
    phase: f32,
    panel: bool,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            if material == MaterialPreset::Frost {
                frost_scene(bounds, window, theme, phase);
            } else {
                storybook_scene(bounds, window, theme, phase, panel);
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn at(bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    point(
        bounds.left() + bounds.size.width * x,
        bounds.top() + bounds.size.height * y,
    )
}

fn paint(window: &mut Window, path: PathBuilder, color: Rgba) {
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// Deliberately non-image detail behind R: crossing ribbons and nested contour lines
/// make actual backdrop sampling visible over arbitrary previously painted geometry.
fn frost_scene(bounds: Bounds<Pixels>, window: &mut Window, theme: &'static Palette, phase: f32) {
    let drift = (phase * TAU).sin() * 0.035;
    for i in 0..5 {
        let x = i as f32 * 0.27 - 0.13;
        let mut ribbon = PathBuilder::fill();
        ribbon.move_to(at(bounds, x, -0.05));
        ribbon.cubic_bezier_to(
            at(bounds, x + 0.25, 1.05),
            at(bounds, x + 0.48 + drift, 0.22),
            at(bounds, x - 0.30, 0.70),
        );
        ribbon.line_to(at(bounds, x + 0.34, 1.05));
        ribbon.cubic_bezier_to(
            at(bounds, x + 0.09, -0.05),
            at(bounds, x - 0.21, 0.70),
            at(bounds, x + 0.57 + drift, 0.22),
        );
        ribbon.close();
        paint(
            window,
            ribbon,
            rgb(if i % 2 == 0 { theme.accent } else { theme.text }).alpha(0.27),
        );
    }
    for i in 0..12 {
        let y = 0.10 + i as f32 * 0.065;
        let mut contour = PathBuilder::stroke(px(if i % 3 == 0 { 3. } else { 1. }));
        contour.move_to(at(bounds, 0., y));
        contour.cubic_bezier_to(
            at(bounds, 1., y + 0.05),
            at(bounds, 0.28, y - 0.17),
            at(bounds, 0.64, y + 0.22),
        );
        paint(window, contour, rgb(theme.text).alpha(0.23));
    }
}

/// Original flat woodland illustration: cut-paper hills, a braided river and a leaf
/// canopy. Five pigment roles only; no borrowed characters, scenes or image assets.
fn storybook_scene(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    theme: &'static Palette,
    phase: f32,
    panel: bool,
) {
    let light = theme.bg > 0x808080;
    let paper = rgb(if light { 0xf3e8cc } else { 0x243c3b });
    let forest = rgb(if light { 0x496b56 } else { 0x648d72 });
    let moss = rgb(if light { 0x9bab70 } else { 0x3e6456 });
    let water = rgb(if light { 0x83aaa0 } else { 0x355d67 });
    let ochre = rgb(if light { 0xbd944a } else { 0xa78c58 });
    let alpha = if panel { 0.16 } else { 0.86 };
    let sway = (phase * TAU).sin();
    if !panel {
        let mut ground = PathBuilder::fill();
        ground.move_to(at(bounds, 0., 0.));
        ground.line_to(at(bounds, 1., 0.));
        ground.line_to(at(bounds, 1., 1.));
        ground.line_to(at(bounds, 0., 1.));
        ground.close();
        paint(window, ground, paper.alpha(0.82));
    }
    for i in 0..3 {
        let y = 0.56 + i as f32 * 0.14;
        let offset = sway * (0.006 + i as f32 * 0.002);
        let mut hill = PathBuilder::fill();
        hill.move_to(at(bounds, -0.02, y));
        hill.cubic_bezier_to(
            at(bounds, 1.02, y + 0.10),
            at(bounds, 0.26, y - 0.28 + offset),
            at(bounds, 0.68, y + 0.22),
        );
        hill.line_to(at(bounds, 1.02, 1.02));
        hill.line_to(at(bounds, -0.02, 1.02));
        hill.close();
        paint(
            window,
            hill,
            if i == 1 {
                forest.alpha(alpha)
            } else {
                moss.alpha(alpha)
            },
        );
    }
    let mut river = PathBuilder::fill();
    river.move_to(at(bounds, 0.59, 0.48));
    river.cubic_bezier_to(
        at(bounds, 0.34, 1.02),
        at(bounds, 0.92, 0.63),
        at(bounds, 0.14, 0.79),
    );
    river.line_to(at(bounds, 0.66, 1.02));
    river.cubic_bezier_to(
        at(bounds, 0.61, 0.48),
        at(bounds, 0.46, 0.81),
        at(bounds, 0.98, 0.65),
    );
    river.close();
    paint(window, river, water.alpha(alpha));
    // Three interlacing current lines, displaced gently as a whole rather than jittered.
    for i in 0..3 {
        let d = i as f32 * 0.022 + sway * 0.003;
        let mut current = PathBuilder::stroke(px(1.));
        current.move_to(at(bounds, 0.61 + d, 0.56));
        current.cubic_bezier_to(
            at(bounds, 0.45 + d, 1.01),
            at(bounds, 0.92 + d, 0.69),
            at(bounds, 0.21 + d, 0.86),
        );
        paint(window, current, paper.alpha(alpha * 0.60));
    }
    // Canopy stems curl around the reading area; leaves are paired pointed curves.
    for side in 0..2 {
        let mirror = |x: f32| if side == 0 { x } else { 1. - x };
        let mut stem = PathBuilder::stroke(px(if panel { 1. } else { 2. }));
        stem.move_to(at(bounds, mirror(0.04), 1.02));
        stem.cubic_bezier_to(
            at(bounds, mirror(0.28), -0.03),
            at(bounds, mirror(0.22), 0.62),
            at(bounds, mirror(-0.09), 0.25),
        );
        paint(window, stem, forest.alpha(alpha));
        for i in 0..10 {
            let y = 0.07 + i as f32 * 0.085;
            let x = 0.065 + 0.06 * (y * 5.).sin();
            let flutter = sway * 0.006 * (1. - y);
            let tip_x = x + if i % 2 == 0 { 0.075 } else { -0.044 };
            let mut leaf = PathBuilder::fill();
            leaf.move_to(at(bounds, mirror(x), y + 0.035));
            leaf.cubic_bezier_to(
                at(bounds, mirror(tip_x + flutter), y - 0.026),
                at(bounds, mirror(x + 0.062), y + 0.022),
                at(bounds, mirror(tip_x + 0.035), y - 0.025),
            );
            leaf.cubic_bezier_to(
                at(bounds, mirror(x), y + 0.035),
                at(bounds, mirror(tip_x - 0.025), y - 0.012),
                at(bounds, mirror(x - 0.025), y + 0.015),
            );
            leaf.close();
            paint(
                window,
                leaf,
                if i % 3 == 0 {
                    ochre.alpha(alpha)
                } else {
                    forest.alpha(alpha)
                },
            );
        }
    }
    // A small solar spiral completes the folktale geometry without adding an asset.
    let mut spiral = PathBuilder::stroke(px(if panel { 0.8 } else { 1.8 }));
    for i in 0..49 {
        let t = i as f32 / 48.;
        let angle = t * TAU * 2.;
        let radius = 0.006 + t * 0.035;
        let p = at(
            bounds,
            0.79 + radius * angle.cos(),
            0.18 + radius * angle.sin(),
        );
        if i == 0 {
            spiral.move_to(p);
        } else {
            spiral.line_to(p);
        }
    }
    paint(window, spiral, ochre.alpha(alpha));
}

/// Fade the composed scroll viewport, not the header, tabs or toolbar.
pub(in crate::gui) fn list_fade(content: impl IntoElement, cx: &App) -> Div {
    let material = MaterialPreset::current(cx);
    let settings = MaterialParameters::current(cx);
    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_hidden()
        .when(
            material.is_some_and(|material| material.is_texture()) && settings.edge_fade > 0.,
            |view| view.group_edge_fade(px(settings.edge_fade * 48.)),
        )
        .child(content)
}

#[cfg(test)]
mod tests {
    use super::ArtworkBlend;
    use gpui::img;

    #[test]
    fn artwork_keeps_previous_visible_until_target_decode_then_dissolves() {
        let mut images = ArtworkBlend {
            previous: Some(img("previous.png")),
            current: None,
            weight: 0.,
        };
        let opacity = 0.58;
        assert_eq!(images.opacities(opacity, true), (opacity, 0.));
        assert_eq!(images.opacities(opacity, false), (0., 0.));

        images.current = Some(img("current.png"));
        assert_eq!(images.opacities(opacity, true), (opacity, 0.));
        for weight in [0.25, 0.5, 0.75, 1.] {
            images.weight = weight;
            let (previous, current) = images.opacities(opacity, true);
            assert_eq!(current, opacity * weight);
            assert!((previous * (1. - current) + current - opacity).abs() < 0.00001);
        }
        assert_eq!(images.opacities(opacity, true), (0., opacity));
        assert_eq!(images.opacities(opacity, false), (0., opacity));
        images.previous = None;
        images.weight = 0.;
        assert_eq!(images.opacities(opacity, true), (0., opacity));
        images.current = None;
        assert_eq!(images.opacities(opacity, true), (0., 0.));
    }
}
