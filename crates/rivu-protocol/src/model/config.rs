//! Field-based configuration edits applied to current owner state.
use rivu_format::config::*;
use std::path::PathBuf;

macro_rules! config_patch {
    ($( $(#[$attr:meta])* pub $field:ident : $ty:ty, )*) => {
        #[derive(Clone, Debug, Default, PartialEq)]
        pub struct ConfigPatch {
            $( $field: Option<$ty>, )*
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

pub enum ConfigChange {
    ToggleVisualization,
    ToggleWaveformSeek,
    ToggleLibraryColumn(LibraryColumn),
    AddLibraryRoot(std::path::PathBuf),
    RemoveLibraryRoot(std::path::PathBuf),
    // Only the cold Settings request allocates; ordinary queue slots stay small.
    Settings(Box<ConfigPatch>),
}
