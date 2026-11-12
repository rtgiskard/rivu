use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable configuration identities, independent of GUI presentation order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialPreset {
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
    pub fn preset(material: MaterialPreset) -> Self {
        static PRESETS: std::sync::LazyLock<BTreeMap<MaterialPreset, MaterialParameters>> =
            std::sync::LazyLock::new(|| {
                let presets: BTreeMap<MaterialPreset, MaterialParameters> =
                    toml::from_str(include_str!("../../data/presets/material.toml"))
                        .expect("embedded material presets must be valid TOML");
                for parameters in presets.values() {
                    parameters
                        .validate()
                        .expect("embedded material preset must be valid");
                }
                presets
            });
        PRESETS[&material]
    }

    pub fn validate(&self) -> Result<()> {
        for parameter in MaterialParameter::ALL {
            let value = parameter.value(*self);
            if parameter.accepts(value) {
                continue;
            }
            let (min, max) = parameter.bounds();
            let name = parameter.field_name();
            ensure!(
                value.is_finite() && (min..=max).contains(&value),
                "interface.material parameter {name} must be finite and between {min} and {max}"
            );
            anyhow::bail!(
                "interface.material color_steps must be 0 or an integer between 2 and 16"
            );
        }
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
    pub const ALL: [Self; 11] = [
        Self::Alpha,
        Self::Feather,
        Self::Highlight,
        Self::Grain,
        Self::Steps,
        Self::Wave,
        Self::CoverBlur,
        Self::BackdropBlur,
        Self::EdgeFade,
        Self::Transition,
        Self::Speed,
    ];

    fn field_name(self) -> &'static str {
        match self {
            Self::Alpha => "panel_alpha",
            Self::Feather => "feather",
            Self::Highlight => "highlight",
            Self::Grain => "grain",
            Self::Steps => "color_steps",
            Self::Wave => "wave",
            Self::CoverBlur => "cover_blur",
            Self::BackdropBlur => "backdrop_blur",
            Self::EdgeFade => "edge_fade",
            Self::Transition => "transition_seconds",
            Self::Speed => "speed",
        }
    }

    pub fn value(self, parameters: MaterialParameters) -> f32 {
        match self {
            Self::Alpha => parameters.panel_alpha,
            Self::Feather => parameters.feather,
            Self::Highlight => parameters.highlight,
            Self::Grain => parameters.grain,
            Self::Steps => parameters.color_steps,
            Self::Wave => parameters.wave,
            Self::CoverBlur => parameters.cover_blur,
            Self::BackdropBlur => parameters.backdrop_blur,
            Self::EdgeFade => parameters.edge_fade,
            Self::Transition => parameters.transition_seconds,
            Self::Speed => parameters.speed,
        }
    }

    pub const fn bounds(self) -> (f32, f32) {
        let max = match self {
            Self::Alpha | Self::Feather | Self::Highlight | Self::Grain | Self::EdgeFade => 1.,
            Self::Steps | Self::Wave => 16.,
            Self::CoverBlur => 24.,
            Self::BackdropBlur => 32.,
            Self::Transition | Self::Speed => 3.,
        };
        (0., max)
    }

    pub fn accepts(self, value: f32) -> bool {
        let (min, max) = self.bounds();
        value.is_finite()
            && (min..=max).contains(&value)
            && (!matches!(self, Self::Steps) || value == 0. || (value >= 2. && value.fract() == 0.))
    }

    /// Assign a raw edit; the owning configuration validates before accepting it.
    pub fn set(self, parameters: &mut MaterialParameters, value: f32) {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_edits_enforce_boundaries_without_changing_other_preferences() {
        for (parameter, maximum) in [
            (MaterialParameter::Alpha, 1.),
            (MaterialParameter::Feather, 1.),
            (MaterialParameter::Highlight, 1.),
            (MaterialParameter::Grain, 1.),
            (MaterialParameter::Steps, 16.),
            (MaterialParameter::Wave, 16.),
            (MaterialParameter::CoverBlur, 24.),
            (MaterialParameter::BackdropBlur, 32.),
            (MaterialParameter::EdgeFade, 1.),
            (MaterialParameter::Transition, 3.),
            (MaterialParameter::Speed, 3.),
        ] {
            for (value, valid) in [
                (f32::NAN, false),
                (f32::NEG_INFINITY, false),
                (f32::INFINITY, false),
                (-1., false),
                (0., true),
                (maximum, true),
                (maximum + 1., false),
                (1., parameter != MaterialParameter::Steps),
                (2.5, maximum >= 2.5 && parameter != MaterialParameter::Steps),
            ] {
                let mut preferences = MaterialPreferences::default();
                let untouched = MaterialParameters::preset(MaterialPreset::Dissolve);
                preferences
                    .presets
                    .insert(MaterialPreset::Dissolve, untouched);
                MaterialChange::SetParameter {
                    preset: MaterialPreset::StillWater,
                    parameter,
                    value,
                }
                .apply_to(&mut preferences);
                let parameters = preferences.presets[&MaterialPreset::StillWater];
                let observed = parameter.value(parameters);
                assert!(observed == value || observed.is_nan() && value.is_nan());
                assert_eq!(
                    parameters.validate().is_ok(),
                    valid,
                    "{parameter:?}: {value}"
                );
                assert_eq!(
                    preferences.validate().is_ok(),
                    valid,
                    "{parameter:?}: {value}"
                );
                assert_eq!(preferences.presets[&MaterialPreset::Dissolve], untouched);
                for other in MaterialParameter::ALL {
                    if other != parameter {
                        assert_eq!(
                            other.value(parameters),
                            other.value(MaterialParameters::preset(MaterialPreset::StillWater))
                        );
                    }
                }
            }
        }
    }
}
