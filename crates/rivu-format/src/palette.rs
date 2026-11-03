//! Toolkit-neutral semantic color representation shared by presentation owners.
/// Semantic 24-bit RGB colors; no alpha or runtime color conversion is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub control: u32,
    pub input: u32,
    pub structure_border: u32,
    pub border: u32,
    pub text: u32,
    pub secondary: u32,
    pub muted: u32,
    pub disabled: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub on_accent: u32,
    pub highlight: u32,
    pub selected: u32,
    pub selected_hover: u32,
    pub error: u32,
    pub error_bg: u32,
    pub warning: u32,
    pub cyan: u32,
    pub lavender: u32,
}
