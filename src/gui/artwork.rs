use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    path::Path,
    rc::{Rc, Weak},
    sync::Arc,
};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity},
    model::Track,
};

use gpui::{
    App, Asset, Context, Entity, EntityId, ImageCache, ImageCacheError, ImageCacheItem, Img,
    ParsedSvg, Pixels, Render, RenderImage, Resource, SvgRenderer, SvgSize, Task, Window, canvas,
    div, img, prelude::*, rgb,
};

use super::GuiApp;

#[derive(Default)]
struct ArtworkImagesState {
    entries: HashMap<Arc<Path>, Weak<ArtworkImageOwner>>,
    decorations: [Weak<DecorationImageOwner>; 6],
    retired: Vec<RetiredArtworkImage>,
    pending_retired: Option<Vec<RetiredArtworkImage>>,
    retirement_generation: u64,
}

/// Shared by one window, never by the application session. Weak entries are
/// removed with their last consumer instead of retaining an idle decode cache.
#[derive(Clone, Default)]
pub(super) struct ArtworkImages(Rc<RefCell<ArtworkImagesState>>);

impl ArtworkImages {
    fn acquire(&self, artwork: LoadedArtwork, cx: &mut App) -> Rc<ArtworkImageOwner> {
        let path = artwork.path();
        if let Some(owner) = self.0.borrow().entries.get(&path).and_then(Weak::upgrade) {
            return owner;
        }
        let owner = Rc::new(ArtworkImageOwner {
            image: ArtworkImage::new(artwork, cx),
            registry: Rc::downgrade(&self.0),
            path: Arc::clone(&path),
        });
        self.0
            .borrow_mut()
            .entries
            .insert(path, Rc::downgrade(&owner));
        owner
    }

    fn retire(&self, image: ArtworkViewImage, cx: &App) {
        // A result never requested by any rendered consumer has no scene.
        if image.owner.image.read(cx).item.is_some() {
            self.0
                .borrow_mut()
                .retired
                .push(RetiredArtworkImage::Artwork(image));
        }
    }

    pub(super) fn decoration(
        &self,
        index: usize,
        bytes: &'static [u8],
        cx: &mut App,
    ) -> ArtworkDecoration {
        let existing = self.0.borrow().decorations[index].upgrade();
        let owner = existing.unwrap_or_else(|| {
            Rc::new(DecorationImageOwner {
                image: DecorationImage::new(bytes, cx),
                registry: Rc::downgrade(&self.0),
                index,
            })
        });
        self.0.borrow_mut().decorations[index] = Rc::downgrade(&owner);
        ArtworkDecoration {
            owner,
            images: self.clone(),
        }
    }

    /// Called by the window root's paint, including after a panel was removed.
    pub(super) fn release_retired(&self, window: &mut Window) {
        let mut state = self.0.borrow_mut();
        if state.retired.is_empty() || state.pending_retired.is_some() {
            return;
        }
        state.pending_retired = Some(std::mem::take(&mut state.retired));
        state.retirement_generation = state.retirement_generation.wrapping_add(1);
        let generation = state.retirement_generation;
        let images = self.clone();
        window.on_next_frame(move |window, cx| {
            let retired = {
                let mut state = images.0.borrow_mut();
                if state.retirement_generation != generation {
                    return;
                }
                state.pending_retired.take().unwrap_or_default()
            };
            for image in retired {
                image.release(window, cx);
            }
        });
    }

    /// Hiding/closing a window cannot wait for an unguaranteed future frame.
    pub(super) fn release_all_retired(&self, window: &mut Window, cx: &mut App) {
        let (retired, pending) = {
            let mut state = self.0.borrow_mut();
            state.retirement_generation = state.retirement_generation.wrapping_add(1);
            (
                std::mem::take(&mut state.retired),
                state.pending_retired.take().unwrap_or_default(),
            )
        };
        for image in retired.into_iter().chain(pending) {
            image.release(window, cx);
        }
    }
}

enum RetiredArtworkImage {
    Artwork(ArtworkViewImage),
    Decoration(Rc<DecorationImageOwner>),
}

impl RetiredArtworkImage {
    fn release(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Artwork(image) => image.owner.release(window, cx),
            Self::Decoration(owner) => owner.release(window, cx),
        }
    }
}

/// A visible style's consumer lease. Scenes hold only the local image entity,
/// never this lease, so the root can retire the last consumer at a frame edge.
pub(super) struct ArtworkDecoration {
    owner: Rc<DecorationImageOwner>,
    images: ArtworkImages,
}

impl ArtworkDecoration {
    pub(super) fn image(&self) -> Img {
        let image = self.owner.image.clone();
        img(move |window: &mut Window, cx: &mut App| {
            image.update(cx, |image, cx| image.load(window, cx))
        })
    }

    pub(super) fn retire(self) {
        self.images
            .0
            .borrow_mut()
            .retired
            .push(RetiredArtworkImage::Decoration(self.owner));
    }

    pub(super) fn release(self, window: &mut Window, cx: &mut App) {
        self.owner.release(window, cx);
    }
}

struct DecorationImageOwner {
    image: Entity<DecorationImage>,
    registry: Weak<RefCell<ArtworkImagesState>>,
    index: usize,
}

impl DecorationImageOwner {
    fn release(self: Rc<Self>, window: &mut Window, cx: &mut App) {
        if Rc::strong_count(&self) == 1 {
            self.image.update(cx, |image, cx| {
                // A scene may retain this entity after the consumer is gone.
                // Released is terminal: it must not start a fresh global load.
                image.task = None;
                let previous = std::mem::replace(&mut image.load, DecorationLoad::Released);
                if let DecorationLoad::Ready(Ok(decoded)) = previous {
                    cx.drop_image(decoded, Some(window));
                }
            });
        }
    }
}

impl Drop for DecorationImageOwner {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut registry = registry.borrow_mut();
            if std::ptr::eq(registry.decorations[self.index].as_ptr(), self) {
                registry.decorations[self.index] = Weak::new();
            }
        }
    }
}

enum DecorationLoad {
    Unrequested,
    Loading(HashSet<EntityId>),
    Ready(Result<Arc<RenderImage>, ImageCacheError>),
    Released,
}

/// A local, cancellable decode. ImageCacheItem accepts Resource, not SVG bytes;
/// keep the equivalent task here rather than putting bytes in the global cache.
struct DecorationImage {
    bytes: &'static [u8],
    load: DecorationLoad,
    // Kept outside load so publishing Ready cannot cancel the publishing task.
    task: Option<Task<()>>,
}

impl DecorationImage {
    fn new(bytes: &'static [u8], cx: &mut App) -> Entity<Self> {
        let image = cx.new(|_| Self {
            bytes,
            load: DecorationLoad::Unrequested,
            task: None,
        });
        cx.observe_release(&image, |image, cx| {
            if let DecorationLoad::Ready(Ok(decoded)) = &image.load {
                cx.drop_image(decoded.clone(), None);
            }
        })
        .detach();
        image
    }

    fn start(
        &mut self,
        future: impl Future<Output = Result<Arc<RenderImage>, ImageCacheError>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_executor().spawn(future);
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            if let Err(error) = &result {
                tracing::error!(?error, "album_decoration_decode_failed");
            }
            let _ = this.update(cx, |image, cx| {
                // Clearing the owner cancels this task. An already-finished
                // worker can never publish into a different style generation.
                if let DecorationLoad::Loading(consumers) = &mut image.load {
                    for consumer in consumers.drain() {
                        App::notify(cx, consumer);
                    }
                    image.load = DecorationLoad::Ready(result);
                }
            });
        });
        self.task = Some(task);
        self.load = DecorationLoad::Loading(HashSet::new());
    }

    fn load(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if matches!(self.load, DecorationLoad::Unrequested) {
            let renderer = cx.svg_renderer();
            let bytes = self.bytes;
            // Same 2x SVG supersampling as Image::to_image_data: these 512px
            // sources remain 1024px, with rotation performed only by the GPU.
            self.start(
                async move { renderer.render_single_frame(bytes, 1.).map_err(Into::into) },
                cx,
            );
        }
        match &mut self.load {
            DecorationLoad::Loading(consumers) => {
                consumers.insert(window.current_view());
                None
            }
            DecorationLoad::Ready(result) => Some(result.clone()),
            DecorationLoad::Unrequested | DecorationLoad::Released => None,
        }
    }
}

/// A consumer lease, deliberately separate from the ImageCache entity held by
/// GPUI elements. Old scenes must not prevent the last consumer from unloading.
struct ArtworkImageOwner {
    image: Entity<ArtworkImage>,
    registry: Weak<RefCell<ArtworkImagesState>>,
    path: Arc<Path>,
}

impl ArtworkImageOwner {
    fn release(self: Rc<Self>, window: &mut Window, cx: &mut App) {
        if Rc::strong_count(&self) == 1 {
            self.image.update(cx, |image, cx| image.clear(window, cx));
        }
    }
}

impl Drop for ArtworkImageOwner {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.borrow_mut().entries.remove(&self.path);
        }
    }
}

#[derive(Clone)]
pub(super) struct ArtworkView {
    state: Rc<RefCell<ArtworkViewState>>,
    images: ArtworkImages,
}

struct ArtworkViewImage {
    // Keep the exact lookup result and its file protection per consumer even
    // when another track's immutable content path shares the decoded image.
    artwork: LoadedArtwork,
    owner: Rc<ArtworkImageOwner>,
}

#[derive(Default)]
struct ArtworkViewState {
    current: Option<ArtworkViewImage>,
}

impl ArtworkView {
    pub(super) fn new(images: ArtworkImages) -> Self {
        Self {
            state: Rc::default(),
            images,
        }
    }

    pub(super) fn replace(&self, next: Option<LoadedArtwork>, cx: &mut App) -> bool {
        let mut state = self.state.borrow_mut();
        if state.current.as_ref().map(|image| &image.artwork) == next.as_ref() {
            return false;
        }
        let next = next.map(|artwork| ArtworkViewImage {
            owner: self.images.acquire(artwork.clone(), cx),
            artwork,
        });
        if let Some(old) = std::mem::replace(&mut state.current, next) {
            self.images.retire(old, cx);
        }
        true
    }

    pub(super) fn image(&self, _: &App) -> Option<Img> {
        let state = self.state.borrow();
        let image = state.current.as_ref()?;
        Some(img(image.artwork.path()).image_cache(&image.owner.image))
    }

    pub(super) fn aspect_ratio(&self, cx: &App) -> Option<f32> {
        let state = self.state.borrow();
        let image = state.current.as_ref()?.owner.image.read(cx);
        let decoded = image.item.as_ref()?.get()?.ok()?;
        let size = decoded.size(0);
        Some(size.width.0 as f32 / size.height.0 as f32)
    }

    pub(super) fn release_all_images(&self, window: &mut Window, cx: &mut App) {
        if let Some(image) = self.state.borrow_mut().current.take() {
            image.owner.release(window, cx);
        }
    }
}

/// One immutable content path and one pending/completed decode per window.
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
    fallback: gpui::SharedString,
    cache: Entity<DefaultArtworkCache>,
    circular: bool,
}

impl Artwork {
    pub(super) fn new(i18n: &crate::i18n::Localizer, cx: &mut App) -> Self {
        Self {
            fallback: i18n.text("artwork-unavailable").to_owned().into(),
            cache: DefaultArtworkCache::new(cx),
            circular: false,
        }
    }

    pub(super) fn circular(&self) -> Self {
        Self {
            fallback: self.fallback.clone(),
            cache: self.cache.clone(),
            circular: true,
        }
    }

    pub(super) fn image(
        &self,
        theme: &'static crate::theme::Palette,
        side: Pixels,
        window: &Window,
    ) -> Img {
        let side = default_artwork_side(side, window.scale_factor());
        let cache = self.cache.clone();
        let fallback = self.fallback.clone();
        img(move |window: &mut Window, cx: &mut App| {
            cache.update(cx, |cache, cx| cache.load(side, window, cx))
        })
        .with_fallback(move || super::caption(theme, fallback.clone()).into_any_element())
    }
}

impl Render for Artwork {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        let cache = self.cache.clone();
        let fallback = self.fallback.clone();
        div()
            .size_full()
            .rounded_md()
            .when(self.circular, |view| view.rounded_full())
            .overflow_hidden()
            .bg(rgb(theme.bg))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let artwork = Artwork {
                            fallback,
                            cache,
                            circular: false,
                        };
                        let mut image = artwork
                            .image(theme, bounds.size.width, window)
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

    const VINYL: &[u8] = include_bytes!("../../assets/album_vinyl.svg");

    struct DecorationScene(Option<Img>);

    impl Render for DecorationScene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.take().expect("single-use test scene").size_full()
        }
    }

    fn draw_decoration(window: &mut gpui::VisualTestContext, decoration: &ArtworkDecoration) {
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(128.), gpui::px(128.)),
            |_, cx| {
                cx.new(|_| DecorationScene(Some(decoration.image())))
                    .into_any_element()
            },
        );
    }

    fn decoded_decoration(
        window: &mut gpui::VisualTestContext,
        image: &Entity<DecorationImage>,
    ) -> std::sync::Weak<RenderImage> {
        window.update(|_, cx| match &image.read(cx).load {
            DecorationLoad::Ready(Ok(decoded)) => {
                assert_eq!(
                    decoded.size(0),
                    gpui::size(gpui::DevicePixels(1024), gpui::DevicePixels(1024))
                );
                Arc::downgrade(decoded)
            }
            _ => panic!("decoration must have completed its local SVG decode"),
        })
    }

    #[gpui::test]
    fn decoration_consumers_share_decode_until_the_last_frame_retirement(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_empty_window();
        let images = ArtworkImages::default();
        let (first, second) = window.update(|_, cx| {
            (
                images.decoration(0, VINYL, cx),
                images.decoration(0, VINYL, cx),
            )
        });
        let old_scene = first.image();
        let cache = first.owner.image.clone();
        assert_eq!(cache, second.owner.image);
        draw_decoration(window, &first);
        window.run_until_parked();
        draw_decoration(window, &second);
        let decoded = decoded_decoration(window, &cache);

        first.retire();
        window.update(|window, _| images.release_retired(window));
        assert!(decoded.upgrade().is_some());
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(decoded.upgrade().is_some());

        second.retire();
        window.update(|window, _| images.release_retired(window));
        assert!(decoded.upgrade().is_some());
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(decoded.upgrade().is_none());
        // An old Img still holds the cache entity, but it cannot restart it.
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(128.), gpui::px(128.)),
            |_, _| old_scene.size_full().into_any_element(),
        );
        window.run_until_parked();
        window.update(|_, cx| assert!(matches!(cache.read(cx).load, DecorationLoad::Released)));
    }

    #[gpui::test]
    fn decoration_reopen_before_retirement_reuses_the_pending_owner(cx: &mut gpui::TestAppContext) {
        let window = cx.add_empty_window();
        let images = ArtworkImages::default();
        let first = window.update(|_, cx| images.decoration(0, VINYL, cx));
        let cache = first.owner.image.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        cache.update(window, |image, cx| {
            image.start(
                async move { receiver.await.expect("pending decode remains owned") },
                cx,
            );
        });
        window.run_until_parked();
        first.retire();
        window.update(|window, _| images.release_retired(window));
        let reopened = window.update(|_, cx| images.decoration(0, VINYL, cx));
        assert_eq!(cache, reopened.owner.image);
        window.update(|window, cx| window.simulate_next_frame(cx));
        let decoded = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        let retained = Arc::downgrade(&decoded);
        sender
            .send(Ok(decoded))
            .expect("reopened consumer keeps the task alive");
        window.run_until_parked();
        assert!(retained.upgrade().is_some());
        window.update(|window, cx| reopened.release(window, cx));
        assert!(retained.upgrade().is_none());
    }

    #[gpui::test]
    fn decoration_flush_cancels_pending_and_ignores_old_frame_callbacks(
        cx: &mut gpui::TestAppContext,
    ) {
        for worker_finished in [false, true] {
            let window = cx.add_empty_window();
            let images = ArtworkImages::default();
            let first = window.update(|_, cx| images.decoration(0, VINYL, cx));
            let old_cache = first.owner.image.clone();
            let old_scene = first.image();
            let (sender, receiver) = futures::channel::oneshot::channel();
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            old_cache.update(window, |image, cx| {
                let finished = finished.clone();
                image.start(
                    async move {
                        let result = receiver.await.expect("controlled decode sender is held");
                        finished.store(true, std::sync::atomic::Ordering::SeqCst);
                        result
                    },
                    cx,
                );
            });
            window.run_until_parked();
            let decoded = Arc::new(RenderImage::new([image::Frame::new(
                image::RgbaImage::new(1, 1),
            )]));
            let stale_pixels = Arc::downgrade(&decoded);
            let mut sender = Some(sender);
            if worker_finished {
                sender.take().unwrap().send(Ok(decoded)).unwrap();
                while !finished.load(std::sync::atomic::Ordering::SeqCst) {
                    assert!(window.background_executor.tick());
                }
            } else {
                drop(decoded);
            }
            first.retire();
            window.update(|window, _| images.release_retired(window));
            // Hiding/closing flushes both the pending frame batch and any newer
            // retirements, without waiting for a native frame that may not run.
            let other = window.update(|_, cx| images.decoration(1, VINYL, cx));
            let other_cache = other.owner.image.clone();
            other.retire();
            window.update(|window, cx| images.release_all_retired(window, cx));
            let reopened = window.update(|_, cx| images.decoration(0, VINYL, cx));
            assert_ne!(old_cache, reopened.owner.image);
            window.run_until_parked();
            if let Some(sender) = sender {
                assert!(
                    sender
                        .send(Ok(Arc::new(RenderImage::new([image::Frame::new(
                            image::RgbaImage::new(1, 1)
                        ),]))))
                        .is_err()
                );
            }
            assert!(stale_pixels.upgrade().is_none());
            draw_decoration(window, &reopened);
            window.run_until_parked();
            let new_pixels = decoded_decoration(window, &reopened.owner.image);
            window.update(|window, cx| window.simulate_next_frame(cx));
            assert!(new_pixels.upgrade().is_some());
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(128.), gpui::px(128.)),
                |_, _| old_scene.into_any_element(),
            );
            window.update(|_, cx| {
                assert!(matches!(old_cache.read(cx).load, DecorationLoad::Released));
                assert!(matches!(
                    other_cache.read(cx).load,
                    DecorationLoad::Released
                ));
            });
            window.update(|window, cx| reopened.release(window, cx));
            assert!(new_pixels.upgrade().is_none());
        }
    }

    #[gpui::test]
    fn artwork_panel_visibility_style_and_layout_transfer_real_decorations(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{
            album_artwork::{AlbumArtwork, AlbumArtworkPanel},
            layout::{Axis, Node, Panel},
        };
        use crate::{
            config::{AlbumArtworkOptions, AlbumArtworkStyle, Config},
            model::PlaybackStatus,
        };

        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(crate::artwork::ArtworkManager::new(directory.path()));
        let images = ArtworkImages::default();
        let config = Config::default();
        let artwork = cx.update(|cx| {
            let default = cx.new(|cx| Artwork::new(&crate::i18n::Localizer::new("en"), cx));
            cx.new(|_| AlbumArtwork::new(manager, default, images.clone()))
        });
        let root = |panels: &[(u64, AlbumArtworkStyle, bool)]| Node::Split {
            id: 90,
            axis: Axis::Horizontal,
            boundaries: vec![0.5],
            children: panels
                .iter()
                .map(|&(id, style, visible)| Node::Tabs {
                    id: id + 10,
                    active: if visible { id } else { id + 100 },
                    panels: vec![
                        Panel {
                            id,
                            kind: "album_artwork".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            album_artwork: Some(AlbumArtworkOptions {
                                style,
                                ..(&config).into()
                            }),
                        },
                        Panel {
                            id: id + 100,
                            kind: "queue".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            album_artwork: None,
                        },
                    ],
                })
                .collect(),
        };
        let sync = |root: Option<&Node>, visible, cx: &mut gpui::TestAppContext| {
            artwork.update(cx, |artwork, cx| {
                artwork.sync(None, PlaybackStatus::Stopped, &config, root, visible, cx);
            });
        };
        let initial = root(&[
            (1, AlbumArtworkStyle::Vinyl, true),
            (2, AlbumArtworkStyle::Vinyl, true),
        ]);
        sync(Some(&initial), true, cx);
        let window = cx.add_empty_window();
        let panel = window.new(|cx| AlbumArtworkPanel::new(2, artwork.clone(), cx));
        let draw_panel = |window: &mut gpui::VisualTestContext| {
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(128.), gpui::px(128.)),
                |_, _| panel.clone().into_any_element(),
            );
        };
        draw_panel(window);
        window.run_until_parked();
        let vinyl = images.0.borrow().decorations[0]
            .upgrade()
            .unwrap()
            .image
            .clone();
        let vinyl_pixels = decoded_decoration(window, &vinyl);

        let one_hidden = root(&[
            (1, AlbumArtworkStyle::Vinyl, false),
            (2, AlbumArtworkStyle::Vinyl, true),
        ]);
        sync(Some(&one_hidden), true, &mut window.cx);
        window.update(|window, _| images.release_retired(window));
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(vinyl_pixels.upgrade().is_some());

        let switched = root(&[
            (1, AlbumArtworkStyle::Vinyl, false),
            (2, AlbumArtworkStyle::Glass, true),
        ]);
        sync(Some(&switched), true, &mut window.cx);
        draw_panel(window);
        window.run_until_parked();
        let glass = images.0.borrow().decorations[1]
            .upgrade()
            .unwrap()
            .image
            .clone();
        let glass_pixels = decoded_decoration(window, &glass);
        window.update(|window, _| images.release_retired(window));
        assert!(vinyl_pixels.upgrade().is_some());
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(vinyl_pixels.upgrade().is_none());
        assert!(glass_pixels.upgrade().is_some());

        // Switching to an empty layout retires the last visible style. A whole
        // window hide/close drains it even with an earlier callback outstanding.
        sync(None, true, &mut window.cx);
        window.update(|window, _| images.release_retired(window));
        window.update(|window, cx| {
            artwork.update(cx, |artwork, cx| artwork.release_all_images(window, cx));
            images.release_all_retired(window, cx);
        });
        assert!(glass_pixels.upgrade().is_none());
        sync(Some(&switched), true, &mut window.cx);
        draw_panel(window);
        window.run_until_parked();
        let reopened = images.0.borrow().decorations[1]
            .upgrade()
            .unwrap()
            .image
            .clone();
        assert_ne!(glass, reopened);
        let reopened_pixels = decoded_decoration(window, &reopened);
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(reopened_pixels.upgrade().is_some());
        // The active-current branch (rather than retirement) is also flushed.
        window.update(|window, cx| {
            artwork.update(cx, |artwork, cx| artwork.release_all_images(window, cx));
            images.release_all_retired(window, cx);
        });
        assert!(reopened_pixels.upgrade().is_none());
    }

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
