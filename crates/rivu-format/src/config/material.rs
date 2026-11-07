use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable configuration identities, independent of GUI presentation order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialPreset {
    Original,
    #[default]
    Satin,
    Atmosphere,
    Glass,
    Stage,
    Mist,
    Spring,
    Glint,
    Ripple,
    Mirror,
    Gradient,
    Feather,
    Sheen,
    EdgeFade,
    Aura,
    Water,
    Grain,
    Dissolve,
    Frost,
    Storybook,
    CgWater,
    StillWater,
    Stream,
    Lake,
    Rain,
    Garden,
    Crystal,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialParameters {
    pub panel_alpha: f32,
    pub feather: f32,
    pub highlight: f32,
    pub grain: f32,
    pub color_steps: f32,
    pub wave: f32,
    pub cover_blur: f32,
    pub backdrop_blur: f32,
    pub edge_fade: f32,
    pub transition_seconds: f32,
    pub speed: f32,
}

impl MaterialParameters {
    pub const fn preset(material: MaterialPreset) -> Self {
        let mut settings = Self {
            panel_alpha: 0.58,
            feather: 0.,
            highlight: 0.,
            grain: 0.,
            color_steps: 0.,
            wave: 0.,
            cover_blur: 0.,
            backdrop_blur: 0.,
            edge_fade: 0.,
            transition_seconds: 0.,
            speed: 1.,
        };
        match material {
            MaterialPreset::Feather => {
                settings.panel_alpha = 0.50;
                settings.feather = 0.62;
            }
            MaterialPreset::Sheen => {
                settings.panel_alpha = 0.68;
                settings.highlight = 0.64;
            }
            MaterialPreset::EdgeFade => {
                settings.panel_alpha = 0.72;
                settings.edge_fade = 0.55;
            }
            MaterialPreset::Aura => {
                settings.panel_alpha = 0.54;
                settings.cover_blur = 18.;
            }
            MaterialPreset::Water => {
                settings.panel_alpha = 0.52;
                settings.wave = 7.;
            }
            MaterialPreset::CgWater => {
                settings.panel_alpha = 0.72;
                settings.wave = 7.;
                settings.highlight = 0.65;
            }
            MaterialPreset::StillWater => {
                settings.panel_alpha = 0.45;
                settings.wave = 4.;
                settings.highlight = 0.32;
                settings.speed = 0.55;
            }
            MaterialPreset::Stream => {
                settings.panel_alpha = 0.40;
                settings.wave = 6.;
                settings.highlight = 0.55;
                settings.speed = 0.8;
            }
            MaterialPreset::Lake => {
                settings.panel_alpha = 0.38;
                settings.wave = 4.;
                settings.highlight = 0.4;
                settings.speed = 0.5;
            }
            MaterialPreset::Rain => {
                settings.panel_alpha = 0.42;
                settings.wave = 5.;
                settings.highlight = 0.45;
                settings.speed = 0.85;
            }
            MaterialPreset::Garden => {
                settings.panel_alpha = 0.40;
                settings.wave = 4.5;
                settings.highlight = 0.40;
                settings.speed = 0.55;
            }
            MaterialPreset::Crystal => {
                settings.panel_alpha = 0.32;
                settings.wave = 4.;
                settings.highlight = 0.50;
                settings.speed = 0.6;
            }
            MaterialPreset::Grain => {
                settings.panel_alpha = 0.82;
                settings.grain = 0.18;
            }
            MaterialPreset::Dissolve => settings.transition_seconds = 0.85,
            MaterialPreset::Frost => {
                settings.panel_alpha = 0.28;
                settings.backdrop_blur = 20.;
            }
            MaterialPreset::Storybook => {
                settings.panel_alpha = 0.78;
                settings.grain = 0.10;
                settings.color_steps = 5.;
                settings.wave = 1.2;
                settings.speed = 0.55;
            }
            _ => {}
        }
        settings
    }

    pub fn validate(&self) -> Result<()> {
        for (name, value, max) in [
            ("panel_alpha", self.panel_alpha, 1.),
            ("feather", self.feather, 1.),
            ("highlight", self.highlight, 1.),
            ("grain", self.grain, 1.),
            ("color_steps", self.color_steps, 16.),
            ("wave", self.wave, 16.),
            ("cover_blur", self.cover_blur, 24.),
            ("backdrop_blur", self.backdrop_blur, 32.),
            ("edge_fade", self.edge_fade, 1.),
            ("transition_seconds", self.transition_seconds, 3.),
            ("speed", self.speed, 3.),
        ] {
            ensure!(
                value.is_finite() && (0.0..=max).contains(&value),
                "interface.material parameter {name} must be finite and between 0 and {max}"
            );
        }
        ensure!(
            self.color_steps == 0. || (self.color_steps >= 2. && self.color_steps.fract() == 0.),
            "interface.material color_steps must be 0 or an integer between 2 and 16"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialPreferences {
    pub enabled: bool,
    pub selected: MaterialPreset,
    pub presets: BTreeMap<MaterialPreset, MaterialParameters>,
}

impl Default for MaterialPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            selected: MaterialPreset::Satin,
            presets: BTreeMap::new(),
        }
    }
}

impl MaterialPreferences {
    /// Preserve legacy no-effect selections while preparing a visible preset to enable.
    pub(super) fn normalize(&mut self) {
        if self.selected == MaterialPreset::Original {
            self.enabled = false;
            self.selected = MaterialPreset::Satin;
        }
    }

    pub fn validate(&self) -> Result<()> {
        for parameters in self.presets.values() {
            parameters.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum MaterialParameter {
    Alpha,
    Feather,
    Highlight,
    Grain,
    Steps,
    Wave,
    CoverBlur,
    BackdropBlur,
    EdgeFade,
    Transition,
    Speed,
}

impl MaterialParameter {
    fn set(self, parameters: &mut MaterialParameters, value: f32) {
        match self {
            Self::Alpha => parameters.panel_alpha = value,
            Self::Feather => parameters.feather = value,
            Self::Highlight => parameters.highlight = value,
            Self::Grain => parameters.grain = value,
            Self::Steps => parameters.color_steps = value,
            Self::Wave => parameters.wave = value,
            Self::CoverBlur => parameters.cover_blur = value,
            Self::BackdropBlur => parameters.backdrop_blur = value,
            Self::EdgeFade => parameters.edge_fade = value,
            Self::Transition => parameters.transition_seconds = value,
            Self::Speed => parameters.speed = value,
        }
    }
}

/// An owner-time edit never replaces unrelated material or application preferences.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum MaterialChange {
    SetEnabled(bool),
    Select(MaterialPreset),
    Reset(MaterialPreset),
    SetParameter {
        preset: MaterialPreset,
        parameter: MaterialParameter,
        value: f32,
    },
}

impl MaterialChange {
    pub fn apply_to(self, preferences: &mut MaterialPreferences) {
        preferences.normalize();
        match self {
            Self::SetEnabled(enabled) => preferences.enabled = enabled,
            Self::Select(preset) => preferences.selected = preset,
            Self::Reset(preset) => {
                preferences.presets.remove(&preset);
            }
            Self::SetParameter {
                preset,
                parameter,
                value,
            } => {
                let parameters = preferences
                    .presets
                    .entry(preset)
                    .or_insert_with(|| MaterialParameters::preset(preset));
                parameter.set(parameters, value);
            }
        }
        preferences.normalize();
    }
}
