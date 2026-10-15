use std::{
    cell::RefCell,
    collections::VecDeque,
    hash::{Hash, Hasher},
    rc::Rc,
    sync::Arc,
};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity},
    model::Track,
};

use gpui::{
    App, Asset, Context, Entity, ImageCache, ImageCacheError, ImageCacheItem, Img, ParsedSvg,
    Pixels, Render, RenderImage, Resource, SvgRenderer, SvgSize, Window, canvas, div, img,
    prelude::*, rgb,
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

// Final BGRA images are memory-only. Limit both key count and bytes, including
// pending assets, so resizing cannot queue unbounded unique-size work.
const DEFAULT_ARTWORK_MAX_SIDE: u32 = 2048;
const DEFAULT_ARTWORK_MAX_ENTRIES: usize = 16;
const DEFAULT_ARTWORK_MAX_BYTES: u64 = 32 * 1024 * 1024;

fn default_artwork_side(side: Pixels, scale: f32) -> u32 {
    (f32::from(side).max(0.) * scale)
        .ceil()
        .clamp(1., DEFAULT_ARTWORK_MAX_SIDE as f32) as u32
}

#[derive(Default)]
struct DefaultArtworkSizes {
    recent: VecDeque<u32>,
    bytes: u64,
}

impl DefaultArtworkSizes {
    fn touch(&mut self, side: u32) -> bool {
        let Some(index) = self.recent.iter().position(|&cached| cached == side) else {
            return false;
        };
        self.recent.remove(index);
        self.recent.push_back(side);
        true
    }

    fn evict_for(&mut self, side: u32) -> Option<u32> {
        if self.recent.len() < DEFAULT_ARTWORK_MAX_ENTRIES
            && self.bytes + u64::from(side).pow(2) * 4 <= DEFAULT_ARTWORK_MAX_BYTES
        {
            return None;
        }
        let evicted = self.recent.pop_front()?;
        self.bytes -= u64::from(evicted).pow(2) * 4;
        Some(evicted)
    }

    fn insert(&mut self, side: u32) {
        self.bytes += u64::from(side).pow(2) * 4;
        self.recent.push_back(side);
    }
}

struct DefaultArtworkRasterizer {
    renderer: SvgRenderer,
    // The lock is async and acquired only by background asset futures. There is
    // one raster/filter operation at a time, not one worker per card or size.
    parsed: futures::lock::Mutex<Option<ParsedSvg>>,
}

#[derive(Clone)]
struct DefaultArtworkSource {
    side: u32,
    rasterizer: Arc<DefaultArtworkRasterizer>,
}

impl Hash for DefaultArtworkSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.side.hash(state);
        Arc::as_ptr(&self.rasterizer).hash(state);
    }
}

struct DefaultArtworkBitmap;

impl Asset for DefaultArtworkBitmap {
    type Source = DefaultArtworkSource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        Self::rasterize(source)
    }
}

impl DefaultArtworkBitmap {
    async fn rasterize(source: DefaultArtworkSource) -> Result<Arc<RenderImage>, ImageCacheError> {
        let result = async {
            let mut parsed = source.rasterizer.parsed.lock().await;
            if parsed.is_none() {
                *parsed = Some(
                    source
                        .rasterizer
                        .renderer
                        .parse_svg(include_bytes!("../../assets/default_artwork.svg"))?,
                );
            }
            // Filter at the final physical size, rather than asking the GPU
            // to minify the old 1024px texture without mipmaps. 2x sampling
            // also resolves subpixel SVG strokes before Lanczos filtering.
            let sample_side = (source.side * 2) as i32;
            let sampled = source.rasterizer.renderer.render_parsed(
                parsed.as_ref().expect("parsed default artwork"),
                SvgSize::ExactSize(gpui::size(
                    gpui::DevicePixels(sample_side),
                    gpui::DevicePixels(sample_side),
                )),
            )?;
            // Borrow the BGRA pixels: no full supersampled-buffer copy.
            // The SVG is opaque, so filtering needs no alpha conversion.
            let buffer = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
                sample_side as u32,
                sample_side as u32,
                sampled.as_bytes(0).expect("rasterized default artwork"),
            )
            .expect("square default artwork buffer");
            let filtered = image::imageops::resize(
                &buffer,
                source.side,
                source.side,
                image::imageops::FilterType::Lanczos3,
            );
            Ok(Arc::new(RenderImage::new([image::Frame::new(filtered)])))
        }
        .await;
        if let Err(error) = &result {
            tracing::error!(?error, side = source.side, "default_artwork_raster_failed");
        }
        result
    }
}

struct DefaultArtworkCache {
    sizes: DefaultArtworkSizes,
    rasterizer: Arc<DefaultArtworkRasterizer>,
}

impl DefaultArtworkCache {
    fn new(cx: &mut App) -> Entity<Self> {
        let renderer = cx.svg_renderer();
        let cache = cx.new(|_| Self {
            sizes: DefaultArtworkSizes::default(),
            rasterizer: Arc::new(DefaultArtworkRasterizer {
                renderer,
                parsed: futures::lock::Mutex::new(None),
            }),
        });
        cx.observe_release(&cache, |cache, cx| {
            for &side in &cache.sizes.recent {
                let source = cache.source(side);
                let result = cx.fetch_asset::<DefaultArtworkBitmap>(&source);
                cx.remove_asset::<DefaultArtworkBitmap>(&source);
                if let Some(Ok(image)) = result {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        cache
    }

    fn source(&self, side: u32) -> DefaultArtworkSource {
        DefaultArtworkSource {
            side,
            rasterizer: Arc::clone(&self.rasterizer),
        }
    }

    fn load(
        &mut self,
        side: u32,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if !self.sizes.touch(side) {
            while let Some(evicted) = self.sizes.evict_for(side) {
                let source = self.source(evicted);
                let result = cx.fetch_asset::<DefaultArtworkBitmap>(&source);
                // Removing the Asset cancels a pending load, including waiters
                // for the rasterizer. Immutable keys never replace another size.
                cx.remove_asset::<DefaultArtworkBitmap>(&source);
                if let Some(Ok(image)) = result {
                    // Keep the old scene's image until the frame boundary.
                    window.on_next_frame(move |window, cx| {
                        cx.drop_image(image, Some(window));
                    });
                }
            }
            self.sizes.insert(side);
        }
        window.use_asset::<DefaultArtworkBitmap>(&self.source(side), cx)
    }
}

/// Square/circular consumers share size-keyed immutable background assets.
pub(super) struct Artwork {
    cache: Entity<DefaultArtworkCache>,
    circular: bool,
}

impl Artwork {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            cache: DefaultArtworkCache::new(cx),
            circular: false,
        }
    }

    pub(super) fn circular(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            circular: true,
        }
    }

    pub(super) fn image(&self, side: Pixels, window: &Window) -> Img {
        let side = default_artwork_side(side, window.scale_factor());
        let cache = self.cache.clone();
        img(move |window: &mut Window, cx: &mut App| {
            cache.update(cx, |cache, cx| cache.load(side, window, cx))
        })
        .with_fallback(|| super::caption("Artwork unavailable").into_any_element())
    }
}

impl Render for Artwork {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let cache = self.cache.clone();
        div()
            .size_full()
            .rounded_md()
            .when(self.circular, |view| view.rounded_full())
            .overflow_hidden()
            .bg(rgb(BG))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let artwork = Artwork {
                            cache,
                            circular: false,
                        };
                        let mut image = artwork
                            .image(bounds.size.width, window)
                            .size(bounds.size.width)
                            .into_any_element();
                        let _ = image.prepaint_as_root(
                            bounds.origin,
                            gpui::size(bounds.size.width, bounds.size.width).map(Into::into),
                            window,
                            cx,
                        );
                        image
                    },
                    |_, mut image, window, cx| image.paint(window, cx),
                )
                .size_full()
                .aspect_ratio(1.),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_artwork_uses_physical_size_and_bounds_large_rasters() {
        assert_eq!(default_artwork_side(gpui::px(180.), 1.), 180);
        assert_eq!(default_artwork_side(gpui::px(180.), 2.), 360);
        assert_eq!(default_artwork_side(gpui::px(111.1), 1.25), 139);
        assert_eq!(default_artwork_side(gpui::px(0.), 2.), 1);
        assert_eq!(default_artwork_side(gpui::px(4096.), 2.), 2048);
    }

    fn request_size(sizes: &mut DefaultArtworkSizes, side: u32) {
        if !sizes.touch(side) {
            while sizes.evict_for(side).is_some() {}
            sizes.insert(side);
        }
    }

    #[test]
    fn default_artwork_resize_demand_stays_bounded_and_reuses_hot_sizes() {
        let mut sizes = DefaultArtworkSizes::default();
        for side in 1..=DEFAULT_ARTWORK_MAX_SIDE {
            request_size(&mut sizes, 180);
            request_size(&mut sizes, 512);
            request_size(&mut sizes, side);
            assert!(sizes.recent.len() <= DEFAULT_ARTWORK_MAX_ENTRIES);
            assert!(sizes.bytes <= DEFAULT_ARTWORK_MAX_BYTES);
            assert!(sizes.recent.contains(&180));
            assert!(sizes.recent.contains(&512));
            assert_eq!(
                sizes.bytes,
                sizes
                    .recent
                    .iter()
                    .map(|&side| u64::from(side).pow(2) * 4)
                    .sum::<u64>()
            );
        }
        let bytes = sizes.bytes;
        let count = sizes.recent.len();
        for _ in 0..100 {
            assert!(sizes.touch(180));
            assert!(sizes.touch(512));
        }
        assert_eq!(sizes.bytes, bytes);
        assert_eq!(sizes.recent.len(), count);
    }

    #[test]
    fn default_artwork_evicts_idle_sizes_before_exceeding_byte_budget() {
        let mut sizes = DefaultArtworkSizes::default();
        request_size(&mut sizes, 2048);
        request_size(&mut sizes, 2047);
        request_size(&mut sizes, 180);
        assert!(!sizes.recent.contains(&2048));
        assert!(sizes.recent.contains(&2047));
        assert!(sizes.recent.contains(&180));
        assert!(sizes.bytes <= DEFAULT_ARTWORK_MAX_BYTES);
    }

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
