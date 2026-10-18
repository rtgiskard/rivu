//! Reactive boundaries for panels whose input actions are coordinated by the window.
//! A visual frame can traverse the shell without rebuilding business lists. Root
//! notifications invalidate these panels; playback-only publications invalidate
//! transport and History when its data changes. The weak host reference never
//! keeps a closed window alive.

use crate::gui::{GuiApp, PanelSpec, layout, panel_spec};
use gpui::{prelude::*, *};

pub(in crate::gui) struct FeaturePanel {
    host: WeakEntity<GuiApp>,
    panel: layout::Panel,
    spec: &'static PanelSpec,
    _subscription: Subscription,
}

impl FeaturePanel {
    fn new(
        host: Entity<GuiApp>,
        panel: &layout::Panel,
        spec: &'static PanelSpec,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&host, |_, _, cx| cx.notify());
        Self {
            host: host.downgrade(),
            panel: panel.clone(),
            spec,
            _subscription: subscription,
        }
    }

    fn sync(&mut self, panel: &layout::Panel, cx: &mut Context<Self>) {
        if self.panel != *panel {
            self.panel.clone_from(panel);
            self.spec = panel_spec(&panel.kind).expect("registered panel");
            cx.notify();
        }
    }
}

impl Render for FeaturePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.host
            .update(cx, |host, cx| {
                host.workspace.scroll_views.borrow_mut().begin_panel(self.panel.id);
                let element = (self.spec.render)(host, &self.panel, window, cx);
                host.workspace.scroll_views.borrow_mut().finish_frame();
                element
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl GuiApp {
    pub(in crate::gui) fn render_panel(
        &mut self,
        panel: &layout::Panel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spec = panel_spec(&panel.kind).expect("registered panel");
        if !matches!(
            panel.kind.as_str(),
            "transport" | "library" | "queue" | "playlists" | "history" | "metadata"
        ) {
            self.feature_panels.remove(&panel.id);
            return (spec.render)(self, panel, window, cx);
        }
        let view = self.feature_panels.entry(panel.id).or_insert_with(|| {
            let host = cx.entity();
            cx.new(|cx| FeaturePanel::new(host, panel, spec, cx))
        });
        view.update(cx, |view, cx| view.sync(panel, cx));
        view.clone()
            .cached(StyleRefinement::default().size_full())
            .into_any_element()
    }
    pub(in crate::gui) fn refresh_playback_panels(
        &self,
        history_changed: bool,
        cx: &mut Context<Self>,
    ) {
        for view in self.feature_panels.values() {
            let kind = view.read(cx).panel.kind.as_str();
            if kind == "transport" || (history_changed && kind == "history") {
                view.update(cx, |_, cx| cx.notify());
            }
        }
    }
}
