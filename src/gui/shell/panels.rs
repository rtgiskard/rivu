//! Cached panel instances observe their feature owner, never the window coordinator.
use crate::gui::interaction::{Measured, MeasurementHandle, WindowGeometry};
use crate::gui::{
    WindowView, artwork_panel, history, layout, library, metadata, playback, playlists, queue,
    scrolling,
};
use gpui::{prelude::*, *};
use std::{cell::RefCell, rc::Rc};

// One declaration defines catalog order and the persisted keys/title mapping.
macro_rules! panel_kinds {
    ($($variant:ident => ($key:literal, $title:literal)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(in crate::gui) enum PanelKind {
            $($variant),+
        }

        impl PanelKind {
            pub(in crate::gui) const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub(in crate::gui) fn from_key(key: &str) -> Option<Self> {
                match key {
                    $($key => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub(in crate::gui) fn key(self) -> &'static str {
                match self {
                    $(Self::$variant => $key),+
                }
            }

            pub(in crate::gui) fn title_id(self) -> &'static str {
                match self {
                    $(Self::$variant => $title),+
                }
            }
        }
    };
}

panel_kinds! {
    Transport => ("transport", "shell-panel-playback"),
    Library => ("library", "shell-panel-library"),
    Albums => ("albums", "shell-panel-albums"),
    Queue => ("queue", "shell-panel-queue"),
    Playlists => ("playlists", "shell-panel-playlists"),
    History => ("history", "shell-panel-history"),
    Metadata => ("metadata", "shell-panel-metadata"),
    Artwork => ("artwork", "shell-panel-artwork"),
    AudioProcessing => ("audio_processing", "shell-panel-audio-processing"),
    AudioVisualizer => ("audio_visualizer", "shell-panel-audio-visualizer"),
}

enum Owner {
    Library(Entity<library::Library>),
    Queue(Entity<queue::Queue>),
    Playlists(Entity<playlists::Playlists>),
    History(Entity<history::History>),
    Metadata(Entity<metadata::Metadata>),
    Playback(Entity<playback::Playback>),
    Processing(Entity<crate::gui::audio_processing::AudioProcessing>),
}

pub(in crate::gui) struct PanelView {
    owner: Owner,
    panel: layout::Panel,
    scroll: Rc<RefCell<scrolling::ScrollViews>>,
    geometry: WindowGeometry,
    measurements: Option<(MeasurementHandle, MeasurementHandle)>,
    _subscription: Subscription,
}

impl PanelView {
    fn new(
        owner: Owner,
        panel: &layout::Panel,
        scroll: Rc<RefCell<scrolling::ScrollViews>>,
        geometry: WindowGeometry,
        measurements: Option<(MeasurementHandle, MeasurementHandle)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = match &owner {
            Owner::Library(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::Queue(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::Playlists(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::History(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::Metadata(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::Playback(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
            Owner::Processing(owner) => cx.observe(owner, |_, _, cx| cx.notify()),
        };
        Self {
            owner,
            panel: panel.clone(),
            scroll,
            geometry,
            measurements,
            _subscription: subscription,
        }
    }

    fn sync(
        &mut self,
        panel: &layout::Panel,
        geometry: WindowGeometry,
        measurements: Option<(MeasurementHandle, MeasurementHandle)>,
        cx: &mut Context<Self>,
    ) {
        self.measurements = measurements;
        if self.panel != *panel || self.geometry != geometry {
            self.panel.clone_from(panel);
            self.geometry = geometry;
            cx.notify();
        }
    }
}

fn measurement(handle: MeasurementHandle, ui_scale: f32) -> AnyElement {
    canvas(
        move |bounds, window, _| handle.record(bounds, WindowGeometry::current(window, ui_scale)),
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scroll.borrow_mut().begin_panel(self.panel.id);
        let id = self.panel.id;
        let element = match &self.owner {
            Owner::Library(owner) => {
                owner.update(cx, |owner, cx| owner.render_panel(id, window, cx))
            }
            Owner::Queue(owner) => owner.update(cx, |owner, cx| owner.render_panel(id, window, cx)),
            Owner::Playlists(owner) => {
                owner.update(cx, |owner, cx| owner.render_panel(id, window, cx))
            }
            Owner::History(owner) => {
                owner.update(cx, |owner, cx| owner.render_panel(id, window, cx))
            }
            Owner::Metadata(owner) => {
                owner.update(cx, |owner, cx| owner.render_panel(id, window, cx))
            }
            Owner::Processing(owner) => {
                owner.update(cx, |owner, cx| owner.render_panel(id, window, cx))
            }
            Owner::Playback(owner) => {
                let (seek, volume) = self.measurements.as_ref().expect("transport measurements");
                owner.update(cx, |owner, cx| {
                    owner.render_panel(
                        id,
                        measurement(seek.clone(), self.geometry.ui_scale),
                        measurement(volume.clone(), self.geometry.ui_scale),
                        window,
                        cx,
                    )
                })
            }
        };
        self.scroll.borrow_mut().finish_frame();
        element
    }
}

impl WindowView {
    fn presentation_panel(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let panel = self
            .presentation
            .update(cx, |presentation, cx| presentation.panel(id, cx));
        panel
            .cached(gpui::StyleRefinement::default().size_full())
            .into_any_element()
    }

    pub(in crate::gui) fn render_panel(
        &mut self,
        kind: PanelKind,
        panel: &layout::Panel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = match kind {
            PanelKind::Library => Owner::Library(self.library.clone()),
            PanelKind::Queue => Owner::Queue(self.queue.clone()),
            PanelKind::Playlists => Owner::Playlists(self.playlists.clone()),
            PanelKind::History => Owner::History(self.history.clone()),
            PanelKind::Metadata => Owner::Metadata(self.metadata.clone()),
            PanelKind::Transport => Owner::Playback(self.playback.clone()),
            PanelKind::AudioProcessing => Owner::Processing(self.audio_processing.clone()),
            PanelKind::Albums => return self.album_browser_panel(panel.id, window, cx),
            PanelKind::Artwork => {
                return self
                    .artwork_panels
                    .entry(panel.id)
                    .or_insert_with(|| {
                        cx.new(|cx| {
                            artwork_panel::ArtworkPanel::new(
                                panel.id,
                                self.artwork_runtime.clone(),
                                cx,
                            )
                        })
                    })
                    .clone()
                    .into_any_element();
            }
            PanelKind::AudioVisualizer => {
                return self.presentation_panel(panel.id, cx);
            }
        };
        let geometry = WindowGeometry::current(window, self.state.system.config.ui_scale);
        let measurements = (kind == PanelKind::Transport).then(|| {
            (
                self.workspace
                    .interaction
                    .measurement_handle(Measured::Seek(panel.id)),
                self.workspace
                    .interaction
                    .measurement_handle(Measured::Volume(panel.id)),
            )
        });
        if self
            .panel_views
            .get(&panel.id)
            .is_some_and(|view| view.read(cx).panel.kind != panel.kind)
        {
            self.panel_views.remove(&panel.id);
        }
        let view = self.panel_views.entry(panel.id).or_insert_with(|| {
            cx.new(|cx| {
                PanelView::new(
                    owner,
                    panel,
                    Rc::clone(&self.workspace.scroll_views),
                    geometry,
                    measurements.clone(),
                    cx,
                )
            })
        });
        view.update(cx, |view, cx| view.sync(panel, geometry, measurements, cx));
        view.clone()
            .cached(StyleRefinement::default().size_full())
            .into_any_element()
    }
}
