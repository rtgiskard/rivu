use super::GuiApp;
use gpui::{AnyElement, Context, prelude::*};

impl GuiApp {
    pub(in crate::gui) fn presentation_panel(
        &mut self,
        id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = self
            .presentation
            .update(cx, |presentation, cx| presentation.panel(id, cx));
        if self.state.system.config.visualization_cache {
            panel
                .cached(gpui::StyleRefinement::default().size_full())
                .into_any_element()
        } else {
            // Repaint uncached panels directly rather than replaying their scenes.
            panel.into_any_element()
        }
    }
}
