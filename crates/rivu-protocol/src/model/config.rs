use rivu_format::config::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

macro_rules! config_patch {
    ($( $(#[$attr:meta])* pub $field:ident : $ty:ty, )*) => {
        /// Only explicitly edited Settings fields; applied to owner-time state.
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        pub struct ConfigPatch {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_field")]
                pub(crate) $field: Option<$ty>,
            )*
        }

        impl ConfigPatch {
            pub fn between(baseline: &Config, draft: &Config) -> Self {
                fn changed<T: Clone + PartialEq>(baseline: &T, draft: &T) -> Option<T> {
                    (baseline != draft).then(|| draft.clone())
                }
                Self {
                    $( $field: changed(&baseline.$field, &draft.$field), )*
                }
            }

            pub fn apply_to(self, config: &mut Config) {
                $( if let Some(value) = self.$field { config.$field = value; } )*
            }
        }
    };
}
rivu_format::config_fields!(config_patch);

fn deserialize_field<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConfigMutation {
    ToggleVisualization,
    ToggleWaveformSeek,
    ToggleLibraryColumn(#[serde(with = "library_column")] LibraryColumn),
    AddLibraryRoot(PathBuf),
    RemoveLibraryRoot(PathBuf),
    Settings(Box<ConfigPatch>),
}

mod library_column {
    use super::*;

    #[derive(Serialize, Deserialize)]
    enum Column {
        Artist,
        Album,
        Duration,
        Codec,
        Bitrate,
    }

    pub fn serialize<S: serde::Serializer>(
        value: &LibraryColumn,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            LibraryColumn::Artist => Column::Artist,
            LibraryColumn::Album => Column::Album,
            LibraryColumn::Duration => Column::Duration,
            LibraryColumn::Codec => Column::Codec,
            LibraryColumn::Bitrate => Column::Bitrate,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<LibraryColumn, D::Error> {
        Ok(match Column::deserialize(deserializer)? {
            Column::Artist => LibraryColumn::Artist,
            Column::Album => LibraryColumn::Album,
            Column::Duration => LibraryColumn::Duration,
            Column::Codec => LibraryColumn::Codec,
            Column::Bitrate => LibraryColumn::Bitrate,
        })
    }
}
