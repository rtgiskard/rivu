//! Persisted material preferences with immediate preview; semantic theme colors stay independent.
use crate::gui::shell::panels::PanelKind;
use gpui::{prelude::*, *};
use rivu_format::config::material::MaterialParameter;
use rivu_format::config::{
    MaterialChange, MaterialParameters, MaterialPreferences, MaterialPreset,
};
use rivu_format::palette::Palette;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum MaterialGroup {
    Texture,
    Water,
    Experimental,
}

impl MaterialGroup {
    pub const ALL: [Self; 3] = [Self::Texture, Self::Water, Self::Experimental];

    pub fn label(self) -> &'static str {
        match self {
            Self::Texture => "material-group-texture",
            Self::Water => "material-group-water",
            Self::Experimental => "material-group-experimental",
        }
    }
}

pub(in crate::gui) trait MaterialAppearance: Sized {
    const ALL: [Self; 27];

    fn group(self) -> Option<MaterialGroup>;
    fn spec(self) -> &'static MaterialSpec;
    fn current(cx: &App) -> Self;
    fn label(self) -> &'static str;
    fn is_texture(&self) -> bool;
    fn is_procedural_water(&self) -> bool;
    fn motion(self, speed: f32, window: &Window, cx: &App) -> bool;
    fn motion_enabled(self, speed: f32, reduce_motion: bool, active: bool) -> bool;
    fn backdrop(
        self,
        theme: &'static Palette,
        image: Option<Img>,
        reflections: Option<[Img; 16]>,
        motion: bool,
    ) -> Div;
    fn panel(
        self,
        panel: Stateful<Div>,
        kind: &str,
        theme: &'static Palette,
        image: Option<Img>,
        motion: bool,
    ) -> Stateful<Div>;
    fn floating(self, surface: Stateful<Div>, theme: &'static Palette) -> Stateful<Div>;
}

/// Only experimental materials expose the panel scene through idle application rows.
pub(in crate::gui) fn row_background(
    theme: &'static Palette,
    selected: bool,
    hovered: bool,
    cx: &App,
) -> Rgba {
    if !selected && !hovered && MaterialPreset::current(cx) != MaterialPreset::Original {
        transparent_black().into()
    } else {
        rivu_ui::row_background(theme, selected, hovered)
    }
}

impl MaterialAppearance for MaterialPreset {
    fn group(self) -> Option<MaterialGroup> {
        match self {
            Self::Original => None,
            Self::Satin
            | Self::Atmosphere
            | Self::Glass
            | Self::Stage
            | Self::Mist
            | Self::Frost => Some(MaterialGroup::Texture),
            Self::Ripple | Self::StillWater | Self::Stream | Self::Lake | Self::Crystal => {
                Some(MaterialGroup::Water)
            }
            _ => Some(MaterialGroup::Experimental),
        }
    }

    const ALL: [Self; 27] = {
        let mut ids = [Self::Original; 27];
        let mut index = 0;
        while index < MATERIALS.len() {
            ids[index] = MATERIALS[index].id;
            index += 1;
        }
        ids
    };

    fn spec(self) -> &'static MaterialSpec {
        &MATERIALS[material_slot(self)]
    }

    fn current(cx: &App) -> Self {
        cx.try_global::<MaterialSession>()
            .map(|session| {
                if session.enabled {
                    session.material
                } else {
                    Self::Original
                }
            })
            .unwrap_or(Self::Original)
    }

    fn label(self) -> &'static str {
        self.spec().menu_label
    }

    fn is_texture(&self) -> bool {
        matches!(
            self.spec().family,
            CompositionFamily::Image | CompositionFamily::ProceduralWater
        )
    }

    fn is_procedural_water(&self) -> bool {
        self.spec().family == CompositionFamily::ProceduralWater
    }

    fn motion(self, speed: f32, window: &Window, cx: &App) -> bool {
        self.motion_enabled(speed, cx.reduce_motion(), window.is_window_active())
    }

    fn motion_enabled(self, speed: f32, reduce_motion: bool, active: bool) -> bool {
        (self.is_texture()
            || matches!(
                self,
                Self::Spring | Self::Glint | Self::Ripple | Self::Mirror
            ))
            && speed.is_finite()
            && speed > 0.
            && !reduce_motion
            && active
    }

    fn backdrop(
        self,
        theme: &'static Palette,
        image: Option<Img>,
        reflections: Option<[Img; 16]>,
        motion: bool,
    ) -> Div {
        match self {
            Self::Spring => return super::flow::spring(theme, image, motion),
            Self::Glint => return super::flow::glint(theme, image, motion),
            Self::Ripple => return super::water::ripple(theme, image, motion),
            Self::Mirror => return super::water::mirror(theme, reflections, motion),
            _ => {}
        }
        let mut layer = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .overflow_hidden()
            .bg(linear_gradient(
                135.,
                linear_color_stop(rgb(theme.bg), 0.),
                linear_color_stop(rgb(theme.control), 1.),
            ));
        if self == Self::Glass {
            if let Some(image) = image {
                layer = layer.child(image.size_full().object_fit(ObjectFit::Cover).opacity(0.48));
            }
            layer = layer.child(div().absolute().size_full().bg(rgb(theme.bg).alpha(0.18)));
        }
        layer
    }

    fn panel(
        self,
        panel: Stateful<Div>,
        kind: &str,
        theme: &'static Palette,
        image: Option<Img>,
        motion: bool,
    ) -> Stateful<Div> {
        if self == Self::Original {
            return panel;
        }
        let playback = matches!(
            PanelKind::from_key(kind),
            Some(PanelKind::Transport | PanelKind::Artwork)
        );
        let image_backed = playback && matches!(self, Self::Atmosphere | Self::Stage);
        let light = theme.bg > 0x808080;
        let (top, bottom) = match self {
            Self::Glass => (0.60, 0.78),
            Self::Stage if playback => (0.22, 0.65),
            Self::Atmosphere if playback => (0.48, 0.78),
            Self::Spring if light => (0.78, 0.90),
            Self::Spring => (0.58, 0.76),
            Self::Glint => (1.0, 1.0),
            Self::Ripple => (0.58, 0.78),
            Self::Mirror if playback => (0.38, 0.78),
            Self::Mirror => (0.76, 0.94),
            _ => (0.94, 1.0),
        };
        let border = match self {
            Self::Mist => rgb(theme.structure_border).alpha(0.15),
            Self::Stage if playback => rgb(theme.accent).alpha(0.42),
            Self::Stage => rgb(theme.structure_border).alpha(0.35),
            Self::Spring => rgb(theme.accent).alpha(if light { 0.22 } else { 0.28 }),
            Self::Glint | Self::Ripple => rgb(theme.border).alpha(0.),
            Self::Mirror => rgb(theme.structure_border).alpha(0.35),
            _ => rgb(theme.border).alpha(if light { 0.35 } else { 0.48 }),
        };
        let mut panel = panel.bg(rgb(theme.panel)).border_color(border);
        if matches!(self, Self::Spring | Self::Ripple) {
            panel = panel.rounded_none();
        }
        if image_backed && let Some(image) = image {
            panel = panel.child(image.absolute().size_full().object_fit(ObjectFit::Cover));
        }
        // Paint decorative layers before content. Alpha never applies to text or hitboxes.
        let tint = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(linear_gradient(
                145.,
                linear_color_stop(rgb(theme.control).alpha(top), 0.),
                linear_color_stop(rgb(theme.panel).alpha(bottom), 1.),
            ));
        if matches!(
            self,
            Self::Glass | Self::Spring | Self::Glint | Self::Ripple | Self::Mirror
        ) {
            panel = panel.bg(transparent_black());
        }
        if self != Self::Glint {
            panel = panel.child(tint);
        }
        panel = match self {
            Self::Spring => panel.child(super::flow::spring_panel(theme, playback, motion)),
            Self::Glint => panel.child(super::flow::glint_panel(theme, playback, motion)),
            Self::Ripple => panel.child(super::water::ripple_panel(theme, playback, motion)),
            Self::Mirror => panel.child(super::water::mirror_panel(theme, playback, motion)),
            _ => panel,
        };
        if matches!(
            self,
            Self::Satin | Self::Atmosphere | Self::Glass | Self::Stage
        ) {
            panel = panel.child(div().absolute().top_0().left_0().w_full().h(px(1.)).bg(
                rgb(if light { 0xffffff } else { theme.text }).alpha(if playback {
                    0.12
                } else {
                    0.055
                }),
            ));
        }
        panel
    }

    fn floating(self, surface: Stateful<Div>, theme: &'static Palette) -> Stateful<Div> {
        if self == Self::Original {
            return surface;
        }
        surface
            .bg(linear_gradient(
                155.,
                linear_color_stop(rgb(theme.control), 0.),
                linear_color_stop(rgb(theme.panel), 1.),
            ))
            .border_color(rgb(theme.border).alpha(if self == Self::Mist { 0.35 } else { 0.65 }))
    }
}

pub(in crate::gui) trait MaterialParametersAppearance: Sized {
    fn cg_water_params(
        self,
        material: MaterialPreset,
        phase: f32,
        light_mode: bool,
    ) -> CgWaterParams;
    fn cg_water_motion(self, motion: bool) -> bool;
    fn current(cx: &App) -> Self;
    fn effective_backdrop_blur(self, scale: f32) -> f32;
}

impl MaterialParametersAppearance for MaterialParameters {
    /// Procedural water reuses the shared 0..16 wave range, not image displacement units.
    fn cg_water_params(
        self,
        material: MaterialPreset,
        phase: f32,
        light_mode: bool,
    ) -> CgWaterParams {
        let unit = |value: f32| {
            if value.is_finite() {
                value.clamp(0., 1.)
            } else {
                0.
            }
        };
        CgWaterParams {
            phase,
            amplitude: unit(self.wave / 16.),
            highlight: unit(self.highlight),
            light_mode,
            style: match material {
                MaterialPreset::Stream => CgWaterStyle::Stream,
                MaterialPreset::Lake => CgWaterStyle::Lake,
                MaterialPreset::Rain => CgWaterStyle::Rain,
                MaterialPreset::Garden => CgWaterStyle::Garden,
                MaterialPreset::Crystal => CgWaterStyle::Crystal,
                MaterialPreset::StillWater => CgWaterStyle::Still,
                _ => CgWaterStyle::Perspective,
            },
        }
    }

    fn cg_water_motion(self, motion: bool) -> bool {
        motion && self.speed > 0. && self.wave > 0.
    }

    fn current(cx: &App) -> Self {
        MaterialSession::current(cx).settings
    }

    /// UI values remain logical pixels. The backend accepts at most 32 physical
    /// pixels, so express the effective logical value before painting a panel.
    fn effective_backdrop_blur(self, scale: f32) -> f32 {
        if !scale.is_finite() || scale <= 0. || !self.backdrop_blur.is_finite() {
            return 0.;
        }
        self.backdrop_blur.max(0.).min(32. / scale)
    }
}

pub(in crate::gui) trait MaterialParameterAppearance {
    fn relevant(self, material: MaterialPreset) -> bool;
    fn label(self, material: MaterialPreset) -> &'static str;
    fn limits(self) -> (f32, f32, f32);
}

impl MaterialParameterAppearance for MaterialParameter {
    fn relevant(self, material: MaterialPreset) -> bool {
        !material.is_procedural_water()
            || matches!(
                self,
                Self::Alpha | Self::Wave | Self::Highlight | Self::Speed
            )
    }

    fn label(self, material: MaterialPreset) -> &'static str {
        if material.is_procedural_water() {
            match self {
                Self::Wave => return "texture-cg-wave",
                Self::Highlight => return "texture-cg-highlight",
                _ => {}
            }
        }
        match self {
            Self::Alpha => "texture-alpha",
            Self::Feather => "texture-feather",
            Self::Highlight => "texture-highlight",
            Self::Grain => "texture-grain",
            Self::Steps => "texture-steps",
            Self::Wave => "texture-wave",
            Self::CoverBlur => "texture-cover-blur",
            Self::BackdropBlur => "texture-backdrop-blur",
            Self::EdgeFade => "texture-edge-fade",
            Self::Transition => "texture-transition",
            Self::Speed => "texture-speed",
        }
    }

    fn limits(self) -> (f32, f32, f32) {
        let (min, max) = self.bounds();
        let step = match self {
            Self::Alpha | Self::Feather | Self::Highlight | Self::EdgeFade => 0.05,
            Self::Grain => 0.02,
            Self::Steps | Self::CoverBlur | Self::BackdropBlur => 1.,
            Self::Wave => 0.5,
            Self::Transition | Self::Speed => 0.1,
        };
        (min, max, step)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::gui) enum CompositionFamily {
    Original,
    Surface,
    Image,
    ProceduralWater,
}

pub(in crate::gui) struct MaterialSpec {
    pub id: MaterialPreset,
    pub menu_label: &'static str,
    pub defaults: MaterialParameters,
    pub family: CompositionFamily,
    pub artwork_root: bool,
    pub artwork_playback: bool,
}

/// Fixed GUI slots, explicitly keyed by domain identity rather than enum representation.
/// Exhaustiveness makes adding a domain preset require a deliberate GUI mapping.
fn material_slot(material: MaterialPreset) -> usize {
    match material {
        MaterialPreset::Original => 0,
        MaterialPreset::Satin => 1,
        MaterialPreset::Atmosphere => 2,
        MaterialPreset::Glass => 3,
        MaterialPreset::Stage => 4,
        MaterialPreset::Mist => 5,
        MaterialPreset::Spring => 6,
        MaterialPreset::Glint => 7,
        MaterialPreset::Ripple => 8,
        MaterialPreset::Mirror => 9,
        MaterialPreset::Gradient => 10,
        MaterialPreset::Feather => 11,
        MaterialPreset::Sheen => 12,
        MaterialPreset::EdgeFade => 13,
        MaterialPreset::Aura => 14,
        MaterialPreset::Water => 15,
        MaterialPreset::Grain => 16,
        MaterialPreset::Dissolve => 17,
        MaterialPreset::Frost => 18,
        MaterialPreset::Storybook => 19,
        MaterialPreset::CgWater => 20,
        MaterialPreset::StillWater => 21,
        MaterialPreset::Stream => 22,
        MaterialPreset::Lake => 23,
        MaterialPreset::Rain => 24,
        MaterialPreset::Garden => 25,
        MaterialPreset::Crystal => 26,
    }
}

pub(in crate::gui) const MATERIALS: [MaterialSpec; 27] = [
    MaterialSpec {
        id: MaterialPreset::Original,
        menu_label: "material-original",
        defaults: MaterialParameters::preset(MaterialPreset::Original),
        family: CompositionFamily::Original,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Satin,
        menu_label: "material-satin",
        defaults: MaterialParameters::preset(MaterialPreset::Satin),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Atmosphere,
        menu_label: "material-atmosphere",
        defaults: MaterialParameters::preset(MaterialPreset::Atmosphere),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Glass,
        menu_label: "material-glass",
        defaults: MaterialParameters::preset(MaterialPreset::Glass),
        family: CompositionFamily::Surface,
        artwork_root: true,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Stage,
        menu_label: "material-stage",
        defaults: MaterialParameters::preset(MaterialPreset::Stage),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Mist,
        menu_label: "material-mist",
        defaults: MaterialParameters::preset(MaterialPreset::Mist),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Spring,
        menu_label: "material-spring",
        defaults: MaterialParameters::preset(MaterialPreset::Spring),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Glint,
        menu_label: "material-glint",
        defaults: MaterialParameters::preset(MaterialPreset::Glint),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Ripple,
        menu_label: "material-ripple",
        defaults: MaterialParameters::preset(MaterialPreset::Ripple),
        family: CompositionFamily::Surface,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Mirror,
        menu_label: "material-mirror",
        defaults: MaterialParameters::preset(MaterialPreset::Mirror),
        family: CompositionFamily::Surface,
        artwork_root: true,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Gradient,
        menu_label: "material-gradient",
        defaults: MaterialParameters::preset(MaterialPreset::Gradient),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Feather,
        menu_label: "material-feather",
        defaults: MaterialParameters::preset(MaterialPreset::Feather),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Sheen,
        menu_label: "material-sheen",
        defaults: MaterialParameters::preset(MaterialPreset::Sheen),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::EdgeFade,
        menu_label: "material-edge-fade",
        defaults: MaterialParameters::preset(MaterialPreset::EdgeFade),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Aura,
        menu_label: "material-aura",
        defaults: MaterialParameters::preset(MaterialPreset::Aura),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Water,
        menu_label: "material-water",
        defaults: MaterialParameters::preset(MaterialPreset::Water),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Grain,
        menu_label: "material-grain",
        defaults: MaterialParameters::preset(MaterialPreset::Grain),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Dissolve,
        menu_label: "material-dissolve",
        defaults: MaterialParameters::preset(MaterialPreset::Dissolve),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::Frost,
        menu_label: "material-frost",
        defaults: MaterialParameters::preset(MaterialPreset::Frost),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Storybook,
        menu_label: "material-storybook",
        defaults: MaterialParameters::preset(MaterialPreset::Storybook),
        family: CompositionFamily::Image,
        artwork_root: true,
        artwork_playback: true,
    },
    MaterialSpec {
        id: MaterialPreset::CgWater,
        menu_label: "material-cg-water",
        defaults: MaterialParameters::preset(MaterialPreset::CgWater),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::StillWater,
        menu_label: "material-still-water",
        defaults: MaterialParameters::preset(MaterialPreset::StillWater),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Stream,
        menu_label: "material-stream",
        defaults: MaterialParameters::preset(MaterialPreset::Stream),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Lake,
        menu_label: "material-lake",
        defaults: MaterialParameters::preset(MaterialPreset::Lake),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Rain,
        menu_label: "material-rain",
        defaults: MaterialParameters::preset(MaterialPreset::Rain),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Garden,
        menu_label: "material-garden",
        defaults: MaterialParameters::preset(MaterialPreset::Garden),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
    MaterialSpec {
        id: MaterialPreset::Crystal,
        menu_label: "material-crystal",
        defaults: MaterialParameters::preset(MaterialPreset::Crystal),
        family: CompositionFamily::ProceduralWater,
        artwork_root: false,
        artwork_playback: false,
    },
];

impl MaterialSpec {
    pub(in crate::gui) fn artwork_demand(&self, visible: bool, playback_panel: bool) -> bool {
        visible && (self.artwork_root || self.artwork_playback && playback_panel)
    }

    pub(in crate::gui) fn controls(&self) -> impl Iterator<Item = MaterialParameter> + '_ {
        MaterialParameter::ALL
            .into_iter()
            .filter(move |parameter| self.id.is_texture() && parameter.relevant(self.id))
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::gui) enum MaterialIntent {
    SetEnabled(bool),
    Select(MaterialPreset),
    Reset(MaterialPreset),
    Set {
        material: MaterialPreset,
        parameter: MaterialParameter,
        value: f32,
    },
}

pub(in crate::gui) struct MaterialSession {
    pub enabled: bool,
    pub material: MaterialPreset,
    pub settings: MaterialParameters,
    saved: [Option<MaterialParameters>; MATERIALS.len()],
    pending: usize,
}
impl Default for MaterialSession {
    fn default() -> Self {
        Self {
            enabled: false,
            material: MaterialPreset::Satin,
            settings: MaterialPreset::Satin.spec().defaults,
            saved: [None; MATERIALS.len()],
            pending: 0,
        }
    }
}
impl Global for MaterialSession {}
impl MaterialSession {
    pub(in crate::gui) fn restore(&mut self, previous: MaterialSnapshot) {
        self.enabled = previous.enabled;
        self.material = previous.material;
        self.settings = previous.settings;
    }

    pub(in crate::gui) fn from_preferences(preferences: &MaterialPreferences) -> Self {
        let mut session = Self::default();
        session.load_preferences(preferences);
        session
    }

    fn load_preferences(&mut self, preferences: &MaterialPreferences) {
        (self.enabled, self.material) = preferences.normalized_selection();
        self.saved.fill(None);
        for (&preset, &parameters) in &preferences.presets {
            self.saved[material_slot(preset)] = Some(parameters);
        }
        self.settings =
            self.saved[material_slot(self.material)].unwrap_or(self.material.spec().defaults);
    }

    pub(in crate::gui) fn reconcile(&mut self, preferences: &MaterialPreferences) {
        // Intermediate config publications cannot replace newer immediate previews.
        if self.pending == 0 {
            self.load_preferences(preferences);
        }
    }

    pub(in crate::gui) fn begin_save(&mut self) {
        self.pending += 1;
    }

    pub(in crate::gui) fn finish_save(&mut self, preferences: &MaterialPreferences) {
        self.pending -= 1;
        self.reconcile(preferences);
    }

    pub(in crate::gui) fn change(&self, intent: MaterialIntent) -> MaterialChange {
        match intent {
            MaterialIntent::SetEnabled(enabled) => MaterialChange::SetEnabled(enabled),
            MaterialIntent::Select(material) => MaterialChange::Select(material),
            MaterialIntent::Reset(material) => MaterialChange::Reset(material),
            MaterialIntent::Set {
                material,
                parameter,
                value,
            } => MaterialChange::SetParameter {
                preset: material,
                parameter,
                value,
            },
        }
    }

    pub(in crate::gui) fn current(cx: &App) -> MaterialSnapshot {
        cx.try_global::<Self>()
            .map(Self::snapshot)
            .unwrap_or_else(|| Self::default().snapshot())
    }

    fn snapshot(&self) -> MaterialSnapshot {
        MaterialSnapshot {
            enabled: self.enabled,
            material: self.material,
            settings: self.settings,
        }
    }

    #[cfg(test)]
    fn modified(&self) -> bool {
        self.snapshot().modified()
    }

    pub(in crate::gui) fn apply(&mut self, intent: MaterialIntent) -> bool {
        match intent {
            MaterialIntent::SetEnabled(enabled) => {
                if self.enabled == enabled {
                    return false;
                }
                self.enabled = enabled;
                true
            }
            MaterialIntent::Select(material) => {
                if self.material == material {
                    return false;
                }
                self.saved[material_slot(self.material)] = Some(self.settings);
                self.material = material;
                self.settings =
                    self.saved[material_slot(material)].unwrap_or(material.spec().defaults);
                true
            }
            MaterialIntent::Reset(material) if material == self.material => {
                let defaults = material.spec().defaults;
                let changed = self.settings != defaults;
                self.settings = defaults;
                changed
            }
            MaterialIntent::Set {
                material,
                parameter,
                value,
            } if material == self.material
                && material.is_texture()
                && parameter.relevant(material)
                && parameter.accepts(value) =>
            {
                if parameter.value(self.settings) == value {
                    return false;
                }
                parameter.set(&mut self.settings, value);
                true
            }
            _ => false,
        }
    }
}

/// Frame sampling copies only active values, never the per-preset preferences.
pub(in crate::gui) struct MaterialSnapshot {
    pub enabled: bool,
    pub material: MaterialPreset,
    pub settings: MaterialParameters,
}

impl MaterialSnapshot {
    pub fn modified(&self) -> bool {
        let defaults = self.material.spec().defaults;
        self.material.spec().controls().any(|parameter| {
            (parameter.value(self.settings) - parameter.value(defaults)).abs()
                > parameter.limits().2 * 0.0001
        })
    }
}

/// Render-local composition facts. Only `sample` may advance Atmosphere;
/// consumers create fresh image elements from these shared source handles.
pub(in crate::gui) struct MaterialFrame {
    pub material: MaterialPreset,
    pub settings: MaterialParameters,
    pub motion: bool,
    previous: Option<std::sync::Arc<RenderImage>>,
    current: Option<std::sync::Arc<RenderImage>>,
    weight: f32,
    surface: Option<crate::gui::artwork::ArtworkSource>,
}

impl MaterialFrame {
    pub(in crate::gui) fn sample(
        atmosphere: &Entity<super::ambient::Atmosphere>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let session = MaterialSession::current(cx);
        let material = MaterialPreset::current(cx);
        let settings = session.settings;
        if !session.enabled {
            return Self {
                material,
                settings,
                motion: false,
                previous: None,
                current: None,
                weight: 1.,
                surface: None,
            };
        }
        let motion = material.motion(settings.speed, window, cx);
        let (previous, current, weight, surface) = atmosphere.update(cx, |atmosphere, cx| {
            if material.spec().family == CompositionFamily::Image {
                let (previous, current, weight) = atmosphere.transition_sources(
                    std::time::Duration::from_secs_f32(settings.transition_seconds),
                    motion,
                    window,
                    cx,
                );
                (previous, current, weight, None)
            } else {
                (None, None, 1., atmosphere.source())
            }
        });
        Self {
            material,
            settings,
            motion,
            previous,
            current,
            weight,
            surface,
        }
    }

    fn images(&self, enabled: bool) -> super::texture_styles::ArtworkBlend {
        super::texture_styles::ArtworkBlend {
            previous: enabled
                .then(|| self.previous.as_ref().map(|source| img(source.clone())))
                .flatten(),
            current: enabled
                .then(|| self.current.as_ref().map(|source| img(source.clone())))
                .flatten(),
            weight: self.weight,
        }
    }

    pub(in crate::gui) fn backdrop(&self, theme: &'static Palette) -> Option<Div> {
        let material = self.material;
        if material == MaterialPreset::Original {
            return None;
        }
        if material.is_texture() {
            return Some(super::texture_styles::backdrop(
                material,
                self.settings,
                theme,
                self.images(material.spec().artwork_root),
                self.motion,
            ));
        }
        let image = (material != MaterialPreset::Mirror)
            .then(|| self.surface.as_ref().map(|source| source.ambient_image()))
            .flatten();
        let reflections = (material == MaterialPreset::Mirror)
            .then(|| {
                self.surface
                    .as_ref()
                    .map(|source| std::array::from_fn(|_| source.static_image()))
            })
            .flatten();
        Some(material.backdrop(theme, image, reflections, self.motion))
    }

    pub(in crate::gui) fn panel(
        &self,
        panel: Stateful<Div>,
        kind: &str,
        theme: &'static Palette,
        scale: f32,
    ) -> Stateful<Div> {
        let playback = matches!(
            PanelKind::from_key(kind),
            Some(PanelKind::Transport | PanelKind::Artwork)
        );
        if self.material.is_texture() {
            let blur = self.settings.effective_backdrop_blur(scale);
            let panel = if blur > 0. {
                panel.backdrop_blur(px(blur))
            } else {
                panel
            };
            return super::texture_styles::panel(
                self.material,
                self.settings,
                panel,
                kind,
                theme,
                self.images(playback && self.material.spec().artwork_playback),
                self.motion,
            );
        }
        let image = (playback && self.material.spec().artwork_playback)
            .then(|| self.surface.as_ref().map(|source| source.ambient_image()))
            .flatten();
        self.material.panel(panel, kind, theme, image, self.motion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn every_material_restores_its_own_persisted_and_preview_parameters() {
        let mut preferences = MaterialPreferences::default();
        for (index, material) in MaterialPreset::ALL.into_iter().enumerate() {
            assert_eq!(material.spec().id, material);
            assert_eq!(
                material.spec().defaults,
                MaterialParameters::preset(material)
            );
            let mut parameters = MaterialParameters::preset(material);
            parameters.panel_alpha = (index + 1) as f32 / 100.;
            preferences.presets.insert(material, parameters);
        }
        let mut session = MaterialSession::from_preferences(&preferences);
        for material in MaterialPreset::ALL {
            session.apply(MaterialIntent::Select(material));
            assert_eq!(session.settings, preferences.presets[&material]);
            if material.is_texture() {
                assert!(session.apply(MaterialIntent::Set {
                    material,
                    parameter: MaterialParameter::Alpha,
                    value: 0.9,
                }));
            }
        }
        for material in MaterialPreset::ALL {
            session.apply(MaterialIntent::Select(material));
            let expected = if material.is_texture() {
                0.9
            } else {
                preferences.presets[&material].panel_alpha
            };
            assert_eq!(session.settings.panel_alpha, expected);
        }
    }

    #[gpui::test]
    fn missing_or_disabled_material_session_never_paints_domain_default(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            assert_eq!(MaterialPreset::current(cx), MaterialPreset::Original);
            cx.default_global::<MaterialSession>();
            assert_eq!(MaterialPreset::current(cx), MaterialPreset::Original);
            cx.default_global::<MaterialSession>()
                .apply(MaterialIntent::SetEnabled(true));
            assert_eq!(MaterialPreset::current(cx), MaterialPreset::Satin);
        });
    }

    #[test]
    fn legacy_original_session_preserves_satin_overrides_without_enabling_effects() {
        let mut preferences = MaterialPreferences {
            selected: MaterialPreset::Original,
            enabled: true,
            ..Default::default()
        };
        let mut satin = MaterialParameters::preset(MaterialPreset::Satin);
        satin.panel_alpha = 0.37;
        preferences.presets.insert(MaterialPreset::Satin, satin);
        let mut session = MaterialSession::from_preferences(&preferences);
        assert_eq!(session.material, MaterialPreset::Satin);
        assert!(!session.enabled);
        assert_eq!(session.settings.panel_alpha, 0.37);
        assert!(session.apply(MaterialIntent::SetEnabled(true)));
        session
            .change(MaterialIntent::SetEnabled(true))
            .apply_to(&mut preferences);
        session.reconcile(&preferences);
        assert!(session.enabled);
        assert_eq!(session.material, MaterialPreset::Satin);
        assert_eq!(session.settings.panel_alpha, 0.37);
    }

    #[test]
    fn persisted_material_initialization_and_pending_preview_reconcile() {
        let mut preferences = MaterialPreferences::default();
        MaterialChange::Select(MaterialPreset::StillWater).apply_to(&mut preferences);
        MaterialChange::SetEnabled(false).apply_to(&mut preferences);
        MaterialChange::SetParameter {
            preset: MaterialPreset::StillWater,
            parameter: rivu_format::config::material::MaterialParameter::Wave,
            value: 9.,
        }
        .apply_to(&mut preferences);
        let mut session = MaterialSession::from_preferences(&preferences);
        assert_eq!(session.material, MaterialPreset::StillWater);
        assert!(!session.enabled);
        assert_eq!(session.settings.wave, 9.);
        for (parameter, value) in [
            (MaterialParameter::Wave, 9.5),
            (MaterialParameter::Speed, 0.8),
        ] {
            let intent = MaterialIntent::Set {
                material: MaterialPreset::StillWater,
                parameter,
                value,
            };
            assert!(session.apply(intent));
            session.begin_save();
            session.change(intent).apply_to(&mut preferences);
        }
        let preview = session.settings;
        session.reconcile(&MaterialPreferences::default());
        assert_eq!(session.settings, preview);
        session.finish_save(&MaterialPreferences::default());
        assert_eq!(session.settings, preview);
        session.finish_save(&preferences);
        assert_eq!(session.settings, preview);
        assert_eq!(
            MaterialSession::from_preferences(&preferences).settings,
            preview
        );
    }

    #[test]
    fn registered_artwork_panel_receives_playback_stage_treatment() {
        let theme = rivu_presentation::theme::palette(rivu_presentation::theme::Theme::RivuDark);
        let frame = MaterialFrame {
            material: MaterialPreset::Stage,
            settings: MaterialPreset::Stage.spec().defaults,
            motion: false,
            previous: None,
            current: None,
            weight: 1.,
            surface: None,
        };
        let panel = |kind: PanelKind| frame.panel(div().id(kind.key()), kind.key(), theme, 1.);
        let mut artwork = panel(PanelKind::Artwork);
        let mut transport = panel(PanelKind::Transport);
        let mut library = panel(PanelKind::Library);
        let mut expected = div().border_color(rgb(theme.accent).alpha(0.42));
        assert_eq!(artwork.style().border_color, expected.style().border_color);
        assert_eq!(artwork.style().border_color, transport.style().border_color);
        assert_ne!(artwork.style().border_color, library.style().border_color);
    }

    #[gpui::test]
    fn only_experimental_idle_rows_are_transparent(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let theme =
                rivu_presentation::theme::palette(rivu_presentation::theme::Theme::RivuDark);
            cx.default_global::<super::MaterialSession>()
                .apply(super::MaterialIntent::SetEnabled(true));
            for material in MaterialPreset::ALL {
                cx.default_global::<super::MaterialSession>()
                    .apply(super::MaterialIntent::Select(material));
                for (selected, hovered) in
                    [(false, false), (false, true), (true, false), (true, true)]
                {
                    let background = super::row_background(theme, selected, hovered, cx);
                    if material == MaterialPreset::Original || selected || hovered {
                        assert_eq!(
                            background,
                            rivu_ui::row_background(theme, selected, hovered)
                        );
                        assert_eq!(background.a, 1.);
                    } else {
                        assert_eq!(background.a, 0.);
                    }
                }
            }
        });
    }

    #[test]
    fn procedural_water_motion_respects_every_static_gate() {
        for water in MaterialPreset::ALL
            .into_iter()
            .filter(|m| m.is_procedural_water())
        {
            assert!(water.motion_enabled(1., false, true));
            assert!(!water.motion_enabled(0., false, true));
            assert!(!water.motion_enabled(-1., false, true));
            assert!(!water.motion_enabled(1., true, true));
            assert!(!water.motion_enabled(1., false, false));
            assert!(!water.motion_enabled(f32::NAN, false, true));
            assert!(!water.motion_enabled(f32::INFINITY, false, true));
        }
    }

    #[test]
    fn requested_backdrop_radius_resolves_at_the_physical_backend_boundary() {
        let mut settings = MaterialPreset::Frost.spec().defaults;
        settings.backdrop_blur = 32.;
        for scale in [1., 1.25, 2., 3.] {
            assert_eq!(settings.effective_backdrop_blur(scale) * scale, 32.);
        }
        settings.backdrop_blur = 4.;
        assert_eq!(settings.effective_backdrop_blur(2.), 4.);
        assert_eq!(settings.effective_backdrop_blur(f32::NAN), 0.);
    }

    #[test]
    fn direct_parameter_values_validate_finite_bounds_and_disabled_values() {
        let mut session = MaterialSession::default();
        session.apply(MaterialIntent::Select(MaterialPreset::Grain));
        for parameter in MaterialParameter::ALL {
            let baseline = session.settings;
            let (min, max, _) = parameter.limits();
            for value in [
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                min - 1.,
                max + 1.,
            ] {
                assert!(!session.apply(MaterialIntent::Set {
                    material: MaterialPreset::Grain,
                    parameter,
                    value,
                }));
                assert_eq!(session.settings, baseline);
            }
            for value in [min, max, 0.] {
                session.apply(MaterialIntent::Set {
                    material: MaterialPreset::Grain,
                    parameter,
                    value,
                });
                assert_eq!(parameter.value(session.settings), value);
                assert!(!session.apply(MaterialIntent::Set {
                    material: MaterialPreset::Grain,
                    parameter,
                    value,
                }));
            }
        }
        for value in [1., 2.5] {
            assert!(!session.apply(MaterialIntent::Set {
                material: MaterialPreset::Grain,
                parameter: MaterialParameter::Steps,
                value,
            }));
        }
        assert!(session.apply(MaterialIntent::Set {
            material: MaterialPreset::Grain,
            parameter: MaterialParameter::Steps,
            value: 2.,
        }));
        let baseline = session.settings;
        assert!(!session.apply(MaterialIntent::Set {
            material: MaterialPreset::Dissolve,
            parameter: MaterialParameter::Alpha,
            value: 0.5,
        }));
        assert_eq!(session.settings, baseline);
        session.apply(MaterialIntent::Select(MaterialPreset::Original));
        assert!(!session.apply(MaterialIntent::Set {
            material: MaterialPreset::Original,
            parameter: MaterialParameter::Alpha,
            value: 0.5,
        }));
    }

    #[test]
    fn resets_restore_cache_and_transition_settings_and_skip_unchanged_sessions() {
        let mut session = MaterialSession::default();
        assert!(!session.apply(MaterialIntent::Select(MaterialPreset::Satin)));
        for (material, parameter) in [
            (MaterialPreset::EdgeFade, MaterialParameter::EdgeFade),
            (MaterialPreset::Dissolve, MaterialParameter::Transition),
        ] {
            assert!(session.apply(MaterialIntent::Select(material)));
            let defaults = session.settings;
            assert!(!session.apply(MaterialIntent::Reset(material)));
            assert!(session.apply(MaterialIntent::Set {
                material,
                parameter,
                value: 0.,
            }));
            assert_ne!(session.settings, defaults);
            assert!(session.apply(MaterialIntent::Reset(material)));
            assert_eq!(session.settings, defaults);
            assert!(!session.apply(MaterialIntent::Select(material)));
            assert!(session.apply(MaterialIntent::Set {
                material,
                parameter,
                value: parameter.limits().1,
            }));
            let edited = session.settings;
            assert!(!session.apply(MaterialIntent::Select(material)));
            assert_eq!(session.settings, edited);
            assert!(session.apply(MaterialIntent::Select(MaterialPreset::Original)));
            assert!(session.apply(MaterialIntent::Select(material)));
            assert_eq!(session.settings, edited);
            assert!(session.apply(MaterialIntent::Reset(material)));
            assert_eq!(session.settings, defaults);
        }
    }

    #[test]
    fn procedural_edits_normalize_units_and_restore_unmodified_status() {
        for material in MaterialPreset::ALL
            .into_iter()
            .filter(|material| material.is_procedural_water())
        {
            let mut session = MaterialSession::default();
            session.apply(MaterialIntent::Select(material));
            let baseline = session.settings;
            assert!(!session.modified());
            for parameter in MaterialParameter::ALL
                .into_iter()
                .filter(|parameter| !parameter.relevant(material))
            {
                assert!(!session.apply(MaterialIntent::Set {
                    material,
                    parameter,
                    value: parameter.limits().1,
                }));
                assert_eq!(session.settings, baseline);
            }
            session.apply(MaterialIntent::Set {
                material,
                parameter: MaterialParameter::Wave,
                value: 12.,
            });
            assert!(session.modified());
            session.apply(MaterialIntent::Set {
                material,
                parameter: MaterialParameter::Wave,
                value: baseline.wave,
            });
            assert!(!session.modified());
            let mut settings = baseline;
            assert!(settings.cg_water_motion(true));
            assert!(!settings.cg_water_motion(false));
            settings.wave = 0.;
            assert!(!settings.cg_water_motion(true));
            settings.wave = 16.;
            settings.speed = 0.;
            assert!(!settings.cg_water_motion(true));
            let params = settings.cg_water_params(material, 0.25, true);
            assert_eq!(params.amplitude, 1.);
            assert_eq!(params.phase, 0.25);
            assert!(params.light_mode);
            settings.wave = -1.;
            settings.highlight = 2.;
            let params = settings.cg_water_params(material, 0., false);
            assert_eq!(params.amplitude, 0.);
            assert_eq!(params.highlight, 1.);
            settings.wave = f32::NAN;
            settings.highlight = f32::INFINITY;
            let params = settings.cg_water_params(material, 0., false);
            assert_eq!(params.amplitude, 0.);
            assert_eq!(params.highlight, 0.);
        }
    }

    #[gpui::test]
    fn sampled_frame_keeps_accepted_sources_and_weight_after_owner_advances(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::gui::{appearance::ambient::Atmosphere, artwork::ArtworkImageRegistry};
        use rivu_metadata::artwork::ArtworkManager;
        use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};
        struct Scene {
            atmosphere: Entity<Atmosphere>,
            observed: Rc<RefCell<Option<MaterialFrame>>>,
        }
        fn composition(frame: &MaterialFrame) -> Div {
            let theme =
                rivu_presentation::theme::palette(rivu_presentation::theme::Theme::RivuDark);
            div()
                .size_full()
                .children(frame.backdrop(theme))
                .child(frame.panel(
                    div().id("material-transport").size_full(),
                    "transport",
                    theme,
                    1.,
                ))
                .child(frame.panel(
                    div().id("material-album").size_full(),
                    PanelKind::Artwork.key(),
                    theme,
                    1.,
                ))
        }
        impl Render for Scene {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let frame = MaterialFrame::sample(&self.atmosphere, window, cx);
                let composition = composition(&frame);
                *self.observed.borrow_mut() = Some(frame);
                composition
            }
        }
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let manager = Arc::new(ArtworkManager::new(directory.path()));
        let tracks: [_; 2] = std::array::from_fn(|index| {
            let mut track = crate::test_support::artwork_track(
                directory.path().join(format!("frame-{index}.wav")),
            );
            track.id = index as i64 + 1;
            rivu_metadata::artwork::test_support::tagged_wav(&track.path);
            manager.load(crate::artwork_source(&track)).unwrap();
            track
        });
        let atmosphere = cx.new(|_| Atmosphere::new(manager, ArtworkImageRegistry::default()));
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            window.activate_window();
            cx.default_global::<MaterialSession>()
                .apply(MaterialIntent::SetEnabled(true));
            cx.default_global::<MaterialSession>()
                .apply(MaterialIntent::Select(MaterialPreset::Dissolve));
            atmosphere.update(cx, |owner, cx| {
                owner.sync(
                    Some(&tracks[0]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                );
            });
        });
        window.run_until_parked();
        let observed = Rc::new(RefCell::new(None));
        let draw = |window: &mut gpui::VisualTestContext| {
            window.draw(point(px(0.), px(0.)), size(px(64.), px(64.)), |_, cx| {
                cx.new(|_| Scene {
                    atmosphere: atmosphere.clone(),
                    observed: observed.clone(),
                })
                .into_any_element()
            });
        };
        draw(window);
        window.run_until_parked();
        draw(window);
        window.update(|_, cx| {
            atmosphere.update(cx, |owner, cx| {
                owner.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Dissolve,
                    1.,
                    true,
                    true,
                    cx,
                )
            })
        });
        draw(window);
        window.run_until_parked();
        draw(window);
        let duration = MaterialPreset::Dissolve.spec().defaults.transition_seconds;
        window
            .executor()
            .advance_clock(Duration::from_secs_f32(duration * 0.5));
        draw(window);
        let accepted = observed.borrow_mut().take().unwrap();
        assert_eq!(accepted.material, MaterialPreset::Dissolve);
        let previous = accepted.previous.as_ref().unwrap().clone();
        let current = accepted.current.as_ref().unwrap().clone();
        assert!((accepted.weight - 0.5).abs() < 0.0001);

        // Complete the owner transition while retaining the previously accepted
        // frame. Retirement must not rewrite the sources or phase a consumer
        // already received, nor let a later material identity leak into it.
        window
            .executor()
            .advance_clock(Duration::from_secs_f32(duration));
        draw(window);
        {
            let observed = observed.borrow();
            let completed = observed.as_ref().unwrap();
            assert!(completed.previous.is_none());
            assert!(completed.current.is_some());
            assert_eq!(completed.weight, 1.);
        }
        window.update(|_, cx| {
            cx.default_global::<MaterialSession>()
                .apply(MaterialIntent::Select(MaterialPreset::Storybook));
            atmosphere.update(cx, |owner, cx| {
                owner.sync(
                    Some(&tracks[1]),
                    MaterialPreset::Storybook,
                    0.,
                    true,
                    true,
                    cx,
                );
            });
        });
        draw(window);
        assert_eq!(
            observed.borrow().as_ref().unwrap().material,
            MaterialPreset::Storybook
        );
        assert_eq!(accepted.material, MaterialPreset::Dissolve);
        assert!(Arc::ptr_eq(accepted.previous.as_ref().unwrap(), &previous));
        assert!(Arc::ptr_eq(accepted.current.as_ref().unwrap(), &current));
        assert!((accepted.weight - 0.5).abs() < 0.0001);
        // Exercise root and both playback consumers of the retained snapshot
        // after the live owner has retired its predecessor and changed identity.
        window.draw(point(px(0.), px(0.)), size(px(64.), px(64.)), |_, _| {
            composition(&accepted).into_any_element()
        });
    }
}
