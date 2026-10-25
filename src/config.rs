//! Application configuration persistence and field-based Settings patches.

pub use rivu_format::config::*;
use std::path::PathBuf;

mod persistence;
pub use persistence::ConfigPersistence;

#[cfg(test)]
mod tests;

macro_rules! config_patch {
    ($( $(#[$attr:meta])* pub $field:ident : $ty:ty, )*) => {
        #[derive(Clone, Debug, Default, PartialEq)]
        pub(crate) struct ConfigPatch {
            $( $field: Option<$ty>, )*
        }

        impl ConfigPatch {
            pub(crate) fn between(baseline: &Config, draft: &Config) -> Self {
                fn changed<T: Clone + PartialEq>(baseline: &T, draft: &T) -> Option<T> {
                    (baseline != draft).then(|| draft.clone())
                }
                Self {
                    $( $field: changed(&baseline.$field, &draft.$field), )*
                }
            }

            pub(crate) fn apply_to(self, config: &mut Config) {
                $( if let Some(value) = self.$field { config.$field = value; } )*
            }
        }
    };
}
rivu_format::config_fields!(config_patch);
