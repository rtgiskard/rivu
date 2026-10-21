//! Cached panel instances observe their feature owner, never the window coordinator.
use crate::gui::layout::interaction::{DockGeometry, Measured, MeasurementHandle};
use crate::gui::{
    GuiApp, album_artwork, history, layout, library, metadata, playback, playlists, queue,
    scrolling,
};
use gpui::{prelude::*, *};
use std::{cell::RefCell, rc::Rc};

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
    geometry: DockGeometry,
    measurements: Option<(MeasurementHandle, MeasurementHandle)>,
    _subscription: Subscription,
}

impl PanelView {
    fn new(
        owner: Owner,
        panel: &layout::Panel,
        scroll: Rc<RefCell<scrolling::ScrollViews>>,
        geometry: DockGeometry,
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
        geometry: DockGeometry,
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
        move |bounds, window, _| handle.record(bounds, DockGeometry::current(window, ui_scale)),
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

impl GuiApp {
    fn presentation_panel(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
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

    pub(in crate::gui) fn render_panel(
        &mut self,
        panel: &layout::Panel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owner = match panel.kind.as_str() {
            "library" => Owner::Library(self.library.clone()),
            "queue" => Owner::Queue(self.queue.clone()),
            "playlists" => Owner::Playlists(self.playlists.clone()),
            "history" => Owner::History(self.history.clone()),
            "metadata" => Owner::Metadata(self.metadata.clone()),
            "transport" => Owner::Playback(self.playback.clone()),
            "audio_processing" => Owner::Processing(self.audio_processing.clone()),
            "albums" => return self.album_browser_panel(panel.id, window, cx),
            "album_artwork" => {
                return self
                    .artwork_panels
                    .entry(panel.id)
                    .or_insert_with(|| {
                        cx.new(|cx| {
                            album_artwork::AlbumArtworkPanel::new(
                                panel.id,
                                self.album_artwork.clone(),
                                cx,
                            )
                        })
                    })
                    .clone()
                    .into_any_element();
            }
            "spectrum" | "spectrogram" | "waveform" | "audio_visualizer" => {
                return self.presentation_panel(panel.id, cx);
            }
            _ => unreachable!("registered panel kind"),
        };
        let geometry = DockGeometry::current(window, self.state.system.config.ui_scale);
        let measurements = (panel.kind == "transport").then(|| {
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
