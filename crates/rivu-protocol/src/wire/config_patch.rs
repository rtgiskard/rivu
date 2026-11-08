use super::WireSpectrumBarLayout;
use crate::model::{ConfigMutation, ConfigPatch};
use rivu_format::config::LibraryColumn;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub enum WireConfigMutation {
    ToggleVisualization,
    ToggleWaveformSeek,
    ToggleLibraryColumn(WireLibraryColumn),
    AddLibraryRoot(PathBuf),
    RemoveLibraryRoot(PathBuf),
    Settings(Box<WireConfigPatch>),
    Material(rivu_format::config::MaterialChange),
}

#[derive(Serialize, Deserialize)]
pub enum WireLibraryColumn {
    Artist,
    Album,
    Duration,
    Codec,
    Bitrate,
}

impl From<ConfigMutation> for WireConfigMutation {
    fn from(value: ConfigMutation) -> Self {
        match value {
            ConfigMutation::ToggleVisualization => Self::ToggleVisualization,
            ConfigMutation::ToggleWaveformSeek => Self::ToggleWaveformSeek,
            ConfigMutation::ToggleLibraryColumn(column) => {
                Self::ToggleLibraryColumn(match column {
                    LibraryColumn::Artist => WireLibraryColumn::Artist,
                    LibraryColumn::Album => WireLibraryColumn::Album,
                    LibraryColumn::Duration => WireLibraryColumn::Duration,
                    LibraryColumn::Codec => WireLibraryColumn::Codec,
                    LibraryColumn::Bitrate => WireLibraryColumn::Bitrate,
                })
            }
            ConfigMutation::AddLibraryRoot(path) => Self::AddLibraryRoot(path),
            ConfigMutation::RemoveLibraryRoot(path) => Self::RemoveLibraryRoot(path),
            ConfigMutation::Settings(patch) => Self::Settings(Box::new((*patch).into())),
            ConfigMutation::Material(change) => Self::Material(change),
        }
    }
}

impl From<WireConfigMutation> for ConfigMutation {
    fn from(value: WireConfigMutation) -> Self {
        match value {
            WireConfigMutation::ToggleVisualization => Self::ToggleVisualization,
            WireConfigMutation::ToggleWaveformSeek => Self::ToggleWaveformSeek,
            WireConfigMutation::ToggleLibraryColumn(column) => {
                Self::ToggleLibraryColumn(match column {
                    WireLibraryColumn::Artist => LibraryColumn::Artist,
                    WireLibraryColumn::Album => LibraryColumn::Album,
                    WireLibraryColumn::Duration => LibraryColumn::Duration,
                    WireLibraryColumn::Codec => LibraryColumn::Codec,
                    WireLibraryColumn::Bitrate => LibraryColumn::Bitrate,
                })
            }
            WireConfigMutation::AddLibraryRoot(path) => Self::AddLibraryRoot(path),
            WireConfigMutation::RemoveLibraryRoot(path) => Self::RemoveLibraryRoot(path),
            WireConfigMutation::Settings(patch) => Self::Settings(Box::new((*patch).into())),
            WireConfigMutation::Material(change) => Self::Material(change),
        }
    }
}

macro_rules! declare_config_patch {
    ($( $(#[$attr:meta])* $field:ident: $ty:ty, )*) => {
        /// Positional optional fields, without human-readable Config serde attributes.
        /// Nested options distinguish unchanged fields from explicitly clearing them.
        #[derive(Serialize, Deserialize)]
        pub struct WireConfigPatch {
            $( $(#[$attr])* $field: Option<$ty>, )*
        }

        impl From<ConfigPatch> for WireConfigPatch {
            fn from(value: ConfigPatch) -> Self {
                Self {
                    $( $field: config_wire_patch_convert!(to_wire, $field, value.$field), )*
                }
            }
        }

        impl From<WireConfigPatch> for ConfigPatch {
            fn from(value: WireConfigPatch) -> Self {
                Self {
                    $( $field: config_wire_patch_convert!(from_wire, $field, value.$field), )*
                }
            }
        }
    };
}

config_wire_fields!(declare_config_patch);
