use std::{cell::RefCell, rc::Rc, sync::Arc};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity},
    model::Track,
};

use gpui::{
    App, Background, Context, Entity, ImageCache, ImageCacheError, ImageCacheItem, Img, Path,
    PathBuilder, Pixels, Render, RenderImage, Resource, Window, canvas, div, img,
    linear_color_stop, linear_gradient, point, prelude::*, px, rgb,
};

use super::{ACCENT, BG, GuiApp};

#[derive(Clone, Default)]
pub(super) struct ArtworkView(Rc<RefCell<ArtworkViewState>>);

#[derive(Default)]
struct ArtworkViewState {
    current: Option<Entity<ArtworkImage>>,
    retired: Vec<Entity<ArtworkImage>>,
}

impl ArtworkView {
    fn replace(&self, next: Option<LoadedArtwork>, cx: &mut App) {
        let mut state = self.0.borrow_mut();
        if state.current.as_ref().map(|image| &image.read(cx).artwork) == next.as_ref() {
            return;
        }
        let next = next.map(|artwork| ArtworkImage::new(artwork, cx));
        if let Some(old) = std::mem::replace(&mut state.current, next) {
            // Results never requested by a rendered preview have no scene to retire.
            if old.read(cx).item.is_some() {
                state.retired.push(old);
            }
        }
    }

    pub(super) fn image(&self, cx: &App) -> Option<Img> {
        let state = self.0.borrow();
        let image = state.current.as_ref()?;
        Some(img(image.read(cx).artwork.path()).image_cache(image))
    }

    pub(super) fn aspect_ratio(&self, cx: &App) -> Option<f32> {
        let state = self.0.borrow();
        let image = state.current.as_ref()?.read(cx);
        let decoded = image.item.as_ref()?.get()?.ok()?;
        let size = decoded.size(0);
        Some(size.width.0 as f32 / size.height.0 as f32)
    }

    pub(super) fn release_retired(&self, window: &mut Window) {
        let retired = std::mem::take(&mut self.0.borrow_mut().retired);
        if !retired.is_empty() {
            window.on_next_frame(move |window, cx| {
                for image in retired {
                    image.update(cx, |image, cx| image.clear(window, cx));
                }
            });
        }
    }

    pub(super) fn release_all_images(&self, window: &mut Window, cx: &mut App) {
        let mut state = self.0.borrow_mut();
        let current = state.current.take();
        for image in current.into_iter().chain(state.retired.drain(..)) {
            image.update(cx, |image, cx| image.clear(window, cx));
        }
    }
}

/// One immutable source per owner. Repeated metadata panels share the same load;
/// another window owns its own decode, so releasing this image cannot evict it.
struct ArtworkImage {
    artwork: LoadedArtwork,
    item: Option<ImageCacheItem>,
}

impl ArtworkImage {
    fn new(artwork: LoadedArtwork, cx: &mut App) -> Entity<Self> {
        let image = cx.new(|_| Self {
            artwork,
            item: None,
        });
        cx.observe_release(&image, |image, cx| {
            if let Some(Ok(decoded)) = image.item.as_ref().and_then(ImageCacheItem::get) {
                cx.drop_image(decoded, None);
            }
            // Dropping the entry also cancels a pending decode. Its source result
            // stays held until then, protecting the file from manual cleanup.
        })
        .detach();
        image
    }

    fn clear(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(item) = self.item.take()
            && let Some(Ok(decoded)) = item.get()
        {
            cx.drop_image(decoded, Some(window));
        }
    }
}

impl ImageCache for ArtworkImage {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        debug_assert_eq!(resource, &Resource::from(self.artwork.path()));
        self.item
            .get_or_insert_with(|| ImageCacheItem::new(resource, cx))
            .use_image(window)
    }
}

/// One executor load plus the latest requested identity, never a task per render.
#[derive(Default)]
pub(super) struct ArtworkLoader {
    active: Option<ArtworkIdentity>,
    pending: Option<Track>,
}

impl ArtworkLoader {
    fn prepare(&mut self, track: Option<&Track>) -> Option<Track> {
        let Some(track) = track else {
            self.pending = None;
            return None;
        };
        if let Some(active) = &self.active {
            if active.matches(track) {
                self.pending = None;
            } else if !self.pending.as_ref().is_some_and(|pending| {
                pending.id == track.id && pending.fingerprint == track.fingerprint
            }) {
                self.pending = Some(track.clone());
            }
            return None;
        }
        self.active = Some(ArtworkIdentity::for_track(track));
        Some(track.clone())
    }

    fn finish(&mut self, identity: &ArtworkIdentity, current: Option<&Track>) -> Option<Track> {
        debug_assert_eq!(self.active.as_ref(), Some(identity));
        self.active = None;
        let pending = self.pending.take()?;
        let current = current?;
        let identity = ArtworkIdentity::for_track(&pending);
        if !identity.matches(current) {
            return None;
        }
        self.active = Some(identity);
        Some(pending)
    }
}

impl GuiApp {
    pub(super) fn prepare_artwork(&mut self, cx: &mut Context<Self>) {
        self.artwork_view.replace(
            self.metadata
                .track()
                .and_then(|track| self.artwork_manager.cached(track)),
            cx,
        );
        let current = self
            .metadata
            .track()
            .filter(|track| !self.artwork_manager.is_loaded(track));
        if let Some(track) = self.artwork_loader.prepare(current) {
            self.start_artwork(track, cx);
        }
    }

    fn start_artwork(&mut self, track: Track, cx: &mut Context<Self>) {
        let identity = ArtworkIdentity::for_track(&track);
        let manager = Arc::clone(&self.artwork_manager);
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { manager.load(&track) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // The URI always comes from the selected track's cache entry;
                // an old completion can warm the cache, never replace the view.
                if this
                    .metadata
                    .track()
                    .is_some_and(|track| identity.matches(track))
                {
                    this.artwork_view.replace(loaded, cx);
                    cx.notify();
                }
                let current = this
                    .metadata
                    .track()
                    .filter(|track| !this.artwork_manager.is_loaded(track));
                if let Some(next) = this.artwork_loader.finish(&identity, current) {
                    this.start_artwork(next, cx);
                } else {
                    this.prepare_artwork(cx);
                }
            });
        })
        .detach();
    }
}

pub(super) struct Artwork {
    layers: Rc<Vec<(Path<Pixels>, Background)>>,
}

impl Artwork {
    pub(super) fn new() -> Self {
        let mut grooves = PathBuilder::stroke(px(1.5));
        for radius in [184., 196., 208., 220.] {
            circle(&mut grooves, 256., 256., radius);
        }
        let mut record = PathBuilder::stroke(px(24.));
        circle(&mut record, 256., 236., 120.);
        let mut inner = PathBuilder::stroke(px(6.));
        circle(&mut inner, 256., 236., 78.);
        let mut spindle = PathBuilder::fill();
        circle(&mut spindle, 256., 236., 16.);
        let layers = vec![
            (build(grooves, false), rgb(ACCENT).alpha(0.09).into()),
            (build(record, true), gradient(ACCENT, 0xbb9af7)),
            (build(inner, true), rgb(ACCENT).alpha(0.28).into()),
            (build(spindle, true), rgb(0x73daca).into()),
            (river(56., false), rgb(BG).into()),
            (river(28., false), gradient(0x7dcfff, 0x73daca)),
            (river(16., true), gradient(0x7dcfff, 0x73daca)),
        ];
        Self {
            layers: Rc::new(layers),
        }
    }
}

impl Render for Artwork {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let layers = Rc::clone(&self.layers);
        div()
            .size_full()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(BG))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let side = bounds.size.width.min(bounds.size.height);
                        let scale = side / px(512.);
                        let origin = bounds.origin
                            + point(
                                (bounds.size.width - side) / 2.,
                                (bounds.size.height - side) / 2.,
                            );
                        // The enclosing cached view also reuses positioned paths
                        // until its bounds or inherited text style change.
                        for (geometry, color) in layers.iter() {
                            let mut path = geometry.clone();
                            path.bounds.origin = origin + path.bounds.origin * scale;
                            path.bounds.size = path.bounds.size.map(|dimension| dimension * scale);
                            for vertex in &mut path.vertices {
                                vertex.xy_position = origin + vertex.xy_position * scale;
                            }
                            window.paint_path(path, *color);
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

fn gradient(from: u32, to: u32) -> Background {
    linear_gradient(
        135.,
        linear_color_stop(rgb(from), 0.),
        linear_color_stop(rgb(to), 1.),
    )
}

fn build(mut builder: PathBuilder, motif: bool) -> Path<Pixels> {
    if motif {
        builder.scale(0.75);
        builder.translate(point(px(64.), px(52.)));
    }
    builder
        .build()
        .expect("fixed artwork geometry must tessellate")
}

fn circle(builder: &mut PathBuilder, x: f32, y: f32, radius: f32) {
    let k = radius * 0.552_284_8;
    let p = |dx, dy| point(px(x + dx), px(y + dy));
    builder.move_to(p(radius, 0.));
    builder.cubic_bezier_to(p(0., radius), p(radius, k), p(k, radius));
    builder.cubic_bezier_to(p(-radius, 0.), p(-k, radius), p(-radius, k));
    builder.cubic_bezier_to(p(0., -radius), p(-radius, -k), p(-k, -radius));
    builder.cubic_bezier_to(p(radius, 0.), p(k, -radius), p(radius, -k));
    builder.close();
}

fn river(width: f32, lower: bool) -> Path<Pixels> {
    let p = |x, y| point(px(x), px(y));
    let mut line = PathBuilder::stroke(px(width * 0.75));
    let endpoints = if lower {
        line.move_to(p(136., 382.));
        line.cubic_bezier_to(p(244., 382.), p(176., 350.), p(208., 354.));
        line.cubic_bezier_to(p(364., 376.), p(280., 410.), p(320., 410.));
        [(136., 382.), (364., 376.)]
    } else {
        line.move_to(p(116., 324.));
        line.cubic_bezier_to(p(244., 324.), p(158., 282.), p(202., 282.));
        line.cubic_bezier_to(p(396., 300.), p(286., 366.), p(330., 366.));
        [(116., 324.), (396., 300.)]
    };
    let mut path = build(line, true);
    let mut caps = PathBuilder::fill();
    for (x, y) in endpoints {
        circle(&mut caps, x, y, width / 2.);
    }
    let caps = build(caps, true);
    path.bounds = path.bounds.union(&caps.bounds);
    path.vertices.extend(caps.vertices);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, fingerprint: &str) -> Track {
        Track {
            id,
            path: format!("/music/{id}.wav").into(),
            fingerprint: Some(fingerprint.into()),
            cue: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: String::new(),
            channels: 0,
            sample_rate: 0,
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            favorite: false,
            decoder_hint: Default::default(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    #[test]
    fn rapid_selection_keeps_only_the_latest_pending_identity() {
        let mut loader = ArtworkLoader::default();
        let first = track(1, "original");
        let skipped = track(2, "original");
        let latest = track(1, "replacement");
        assert!(loader.prepare(Some(&first)).is_some());
        let active = ArtworkIdentity::for_track(&first);
        assert!(loader.prepare(Some(&skipped)).is_none());
        assert!(loader.prepare(Some(&latest)).is_none());
        assert!(loader.prepare(Some(&latest)).is_none());
        assert!(!active.matches(&latest));
        let next = loader.finish(&active, Some(&latest)).unwrap();
        assert_eq!(next.id, latest.id);
        assert_eq!(next.fingerprint, latest.fingerprint);
        assert!(loader.pending.is_none());
    }

    #[test]
    fn returning_to_active_or_clearing_selection_drops_pending_work() {
        let first = track(1, "original");
        let other = track(2, "original");
        for current in [Some(&first), None] {
            let mut loader = ArtworkLoader::default();
            loader.prepare(Some(&first));
            loader.prepare(Some(&other));
            loader.prepare(current);
            assert!(
                loader
                    .finish(&ArtworkIdentity::for_track(&first), current)
                    .is_none()
            );
            assert!(loader.active.is_none());
            assert!(loader.pending.is_none());
        }
    }
}
