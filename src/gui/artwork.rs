use std::{cell::RefCell, rc::Rc, sync::Arc};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity},
    model::Track,
};

use gpui::{
    App, Context, Entity, Image, ImageCache, ImageCacheError, ImageCacheItem, ImageFormat, Img,
    Render, RenderImage, Resource, Window, div, img, prelude::*, rgb,
};

use super::{BG, GuiApp};

#[derive(Clone, Default)]
pub(super) struct ArtworkView(Rc<RefCell<ArtworkViewState>>);

#[derive(Default)]
struct ArtworkViewState {
    current: Option<Entity<ArtworkImage>>,
    retired: Vec<Entity<ArtworkImage>>,
}

impl ArtworkView {
    pub(super) fn replace(&self, next: Option<LoadedArtwork>, cx: &mut App) -> bool {
        let mut state = self.0.borrow_mut();
        if state.current.as_ref().map(|image| &image.read(cx).artwork) == next.as_ref() {
            return false;
        }
        let next = next.map(|artwork| ArtworkImage::new(artwork, cx));
        if let Some(old) = std::mem::replace(&mut state.current, next) {
            // Results never requested by a rendered preview have no scene to retire.
            if old.read(cx).item.is_some() {
                state.retired.push(old);
            }
        }
        true
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
    pub(super) fn prepare(&mut self, track: Option<&Track>) -> Option<Track> {
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

    pub(super) fn finish(
        &mut self,
        identity: &ArtworkIdentity,
        current: Option<&Track>,
    ) -> Option<Track> {
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
                .and_then(|track| self.artwork_manager.cached(track))
                .filter(|artwork| !artwork.is_placeholder()),
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
                    this.artwork_view
                        .replace(loaded.filter(|artwork| !artwork.is_placeholder()), cx);
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

/// The square and circular views share one immutable SVG image identity. GPUI
/// decodes it asynchronously through its asset system, just like other images.
pub(super) struct Artwork {
    source: Arc<Image>,
    circular: bool,
}

impl Artwork {
    pub(super) fn new() -> Self {
        Self {
            source: Arc::new(Image::from_bytes(
                ImageFormat::Svg,
                include_bytes!("../../assets/default_artwork.svg").to_vec(),
            )),
            circular: false,
        }
    }

    pub(super) fn circular(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            circular: true,
        }
    }

    pub(super) fn image(&self) -> Img {
        img(Arc::clone(&self.source))
    }
}

impl Render for Artwork {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let image = self.image();
        div()
            .size_full()
            .rounded_md()
            .when(self.circular, |view| view.rounded_full())
            .overflow_hidden()
            .bg(rgb(BG))
            .child(image.size_full())
            .into_any_element()
    }
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
