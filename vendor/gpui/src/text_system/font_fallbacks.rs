use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Explicit fallback families, optionally followed by platform system matching.
#[derive(Default, Clone, Eq, PartialEq, Hash, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct FontFallbacks {
    fonts: Arc<Vec<String>>,
    #[serde(skip)]
    #[schemars(skip)]
    system_family: Option<crate::SharedString>,
    #[serde(skip)]
    #[schemars(skip)]
    primary_family: Option<crate::SharedString>,
}

impl FontFallbacks {
    /// Get the fallback fonts family names
    pub fn fallback_list(&self) -> &[String] {
        self.fonts.as_slice()
    }

    /// Create a font fallback from a list of strings
    pub fn from_fonts(fonts: Vec<String>) -> Self {
        Self {
            fonts: Arc::new(fonts),
            system_family: None,
            primary_family: None,
        }
    }

    /// Request platform fallback using the original, possibly generic, family.
    pub fn from_system_family(
        family: impl Into<crate::SharedString>,
        primary: impl Into<crate::SharedString>,
    ) -> Self {
        Self {
            fonts: Arc::default(),
            system_family: Some(family.into()),
            primary_family: Some(primary.into()),
        }
    }

    /// Original family for platform matching, excluding inherited icon overrides.
    pub fn system_family(&self, actual_family: &str) -> Option<&str> {
        self.system_family
            .as_deref()
            .filter(|_| self.primary_family.as_deref() == Some(actual_family))
    }
}
