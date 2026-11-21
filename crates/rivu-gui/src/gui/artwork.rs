use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    path::Path,
    rc::{Rc, Weak},
    sync::{Arc, OnceLock},
    time::Duration,
};

use rivu_metadata::artwork::{Artwork as LoadedArtwork, ArtworkIdentity, Source};
use rivu_protocol::model::ArtworkSource as OwnedArtworkSource;

use gpui::{
    App, Asset, Context, Entity, EntityId, ImageCache, ImageCacheError, ImageCacheItem, Img,
    ParsedSvg, Pixels, Render, RenderImage, Resource, SvgRenderer, SvgSize, Task, Window, canvas,
    div, img, prelude::*, rgb,
};

mod ambient;
mod decode;

#[cfg(test)]
pub(super) use decode::tests::gif_with_corrupt_second_frame;

impl gpui::Global for decode::DecodeWorkspace {}

#[derive(Default)]
struct ArtworkImageRegistryState {
    budget: decode::Budget,
    entries: HashMap<(Arc<Path>, usize), Weak<ArtworkImageLease>>,
    decorations: [Weak<DecorationImageLease>; 6],
    retired: Vec<RetiredArtworkImage>,
    pending_retired: Option<Vec<RetiredArtworkImage>>,
    retirement_generation: u64,
    next_consumer: u64,
}

/// Shared by one window, never by the application session. Weak entries are
/// removed with their last consumer instead of retaining an idle decode cache.
#[derive(Clone, Default)]
pub(super) struct ArtworkImageRegistry(Rc<RefCell<ArtworkImageRegistryState>>);

impl ArtworkImageRegistry {
    pub(super) fn new(retained_mib: u32) -> Self {
        Self(Rc::new(RefCell::new(ArtworkImageRegistryState {
            budget: decode::Budget::new(retained_mib),
            ..ArtworkImageRegistryState::default()
        })))
    }

    pub(super) fn set_decode_budget_mib(&self, retained_mib: u32, cx: &mut App) -> bool {
        self.set_decode_budget(decode::Budget::new(retained_mib), cx)
    }

    fn set_decode_budget(&self, budget: decode::Budget, cx: &mut App) -> bool {
        let mut state = self.0.borrow_mut();
        if state.budget == budget {
            return false;
        }
        state.budget = budget;
        let ArtworkImageRegistryState {
            entries, retired, ..
        } = &mut *state;
        for owner in entries.values().filter_map(Weak::upgrade) {
            let decoded = owner.image.update(cx, |image, cx| {
                image.budget = budget;
                image.target = image
                    .demands
                    .values()
                    .copied()
                    .max()
                    .unwrap_or_else(|| decode::requested_tier(0));
                let generation = image.generation.wrapping_add(1);
                let pixels = image.take_guarded_pixels(generation);
                if image.views.is_empty() {
                    pixels.map(|retired| retired.image).collect::<Vec<_>>()
                } else {
                    image.generation = generation;
                    image.replaced.extend(pixels);
                    image.invalidate_consumers(cx);
                    Vec::new()
                }
            });
            for decoded in decoded {
                retired.push(RetiredArtworkImage::Decoded(decoded));
            }
        }
        true
    }

    fn acquire(&self, artwork: LoadedArtwork, cx: &mut App) -> Rc<ArtworkImageLease> {
        let key = (artwork.path(), artwork.max_image_bytes());
        let budget = {
            let state = self.0.borrow();
            if let Some(owner) = state.entries.get(&key).and_then(Weak::upgrade) {
                return owner;
            }
            state.budget
        };
        let owner = Rc::new(ArtworkImageLease {
            image: ArtworkImage::new(artwork, budget, Rc::downgrade(&self.0), cx),
            registry: Rc::downgrade(&self.0),
            key: key.clone(),
        });
        self.0
            .borrow_mut()
            .entries
            .insert(key, Rc::downgrade(&owner));
        owner
    }

    fn retire(&self, image: ArtworkViewImage, cx: &mut App) {
        // Measurement handles can already be held by a scene before Img starts
        // its first decode; those owners must also become terminal at the edge.
        if !image.consumers.is_empty() || image.owner.image.read(cx).item.is_some() {
            self.0
                .borrow_mut()
                .retired
                .push(RetiredArtworkImage::Artwork(image));
        } else if Rc::strong_count(&image.owner) == 1 {
            // A previously built Img may still hold the entity, not its lease.
            image.owner.image.update(cx, |image, _| {
                image.artwork = None;
            });
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
            Rc::new(DecorationImageLease {
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
        // Releasing consumer leases can acknowledge guarded replacements.
        // Those decoded-only retirements cannot enqueue further consumers.
        let acknowledged = std::mem::take(&mut self.0.borrow_mut().retired);
        for image in acknowledged {
            image.release(window, cx);
        }
    }
}

enum RetiredArtworkImage {
    Artwork(ArtworkViewImage),
    Demand(Entity<ArtworkImage>, Vec<u64>),
    Decoration(Rc<DecorationImageLease>),
    Decoded(Arc<RenderImage>),
}

impl RetiredArtworkImage {
    fn release(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Artwork(image) => image.release(window, cx),
            Self::Demand(image, consumers) => image.update(cx, |image, cx| {
                let mut demand_changed = false;
                for consumer in consumers {
                    demand_changed |= image.demands.remove(&consumer).is_some();
                    image.views.remove(&consumer);
                }
                image.flush_replaced();
                if demand_changed {
                    image.reconcile_demand(window, cx);
                }
            }),
            Self::Decoration(owner) => owner.release(window, cx),
            Self::Decoded(decoded) => cx.drop_image(decoded, Some(window)),
        }
    }
}

/// A visible style's consumer lease. Scenes hold only the local image entity,
/// never this lease, so the root can retire the last consumer at a frame edge.
pub(super) struct ArtworkDecoration {
    owner: Rc<DecorationImageLease>,
    images: ArtworkImageRegistry,
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

struct DecorationImageLease {
    image: Entity<DecorationImage>,
    registry: Weak<RefCell<ArtworkImageRegistryState>>,
    index: usize,
}

impl DecorationImageLease {
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

impl Drop for DecorationImageLease {
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
struct ArtworkImageLease {
    image: Entity<ArtworkImage>,
    registry: Weak<RefCell<ArtworkImageRegistryState>>,
    key: (Arc<Path>, usize),
}

impl ArtworkImageLease {
    fn release(self: Rc<Self>, window: &mut Window, cx: &mut App) {
        if Rc::strong_count(&self) == 1 {
            self.image.update(cx, |image, cx| image.clear(window, cx));
        }
    }
}

impl Drop for ArtworkImageLease {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.borrow_mut().entries.remove(&self.key);
        }
    }
}

#[derive(Clone)]
pub(super) struct ArtworkView {
    state: Rc<RefCell<ArtworkViewState>>,
    images: ArtworkImageRegistry,
}

struct ArtworkViewImage {
    // Keep the exact lookup result and its file protection per consumer even
    // when another track's immutable content path shares the decoded image.
    artwork: LoadedArtwork,
    owner: Rc<ArtworkImageLease>,
    consumers: HashMap<u64, u64>,
}

impl ArtworkViewImage {
    fn release(self, window: &mut Window, cx: &mut App) {
        if !self.consumers.is_empty() {
            self.owner.image.update(cx, |image, cx| {
                let mut demand_changed = false;
                for consumer in self.consumers.into_values() {
                    demand_changed |= image.demands.remove(&consumer).is_some();
                    image.views.remove(&consumer);
                }
                image.flush_replaced();
                // A scene handle that was never measured owns no demand to
                // remove. Its retirement must not unload another live lease.
                if demand_changed {
                    image.reconcile_demand(window, cx);
                }
            });
        }
        self.owner.release(window, cx);
    }
}

#[derive(Default)]
struct ArtworkViewState {
    current: Option<ArtworkViewImage>,
}

/// Scene-held measurement handle, deliberately not a consumer/source lease.
#[derive(Clone)]
pub(super) struct ArtworkImageSizing {
    image: Entity<ArtworkImage>,
    token: u64,
    max_side: Option<u32>,
}

impl ArtworkImageSizing {
    fn register_demand(
        &self,
        image: &mut ArtworkImage,
        side: Pixels,
        window: &mut Window,
        cx: &mut Context<ArtworkImage>,
    ) {
        if image.artwork.is_none() {
            return;
        }
        image
            .views
            .insert(self.token, (window.current_view(), image.generation));
        image.flush_replaced();
        image.request(self.token, side, self.max_side, window, cx);
    }

    /// Register measured demand before subscribing to decode completion.
    pub(super) fn request_image(
        &self,
        side: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Img> {
        let decoded = self
            .image
            .update(cx, |image, cx| {
                self.register_demand(image, side, window, cx);
                let path = image.artwork.as_ref()?.path();
                image.load(&Resource::from(path), window, cx)
            })?
            .ok()?;
        if decoded.frame_count() == 0 {
            return None;
        }
        let mut element = img(decoded);
        element.interactivity().element_id = Some(("artwork-cover", self.image.entity_id()).into());
        Some(element)
    }

    fn request_cover(&self, window: &mut Window, cx: &mut App) {
        self.image.update(cx, |image, cx| {
            let side = image.cover_side(window.viewport_size());
            self.register_demand(image, side, window, cx);
        });
    }
}

/// A frame-local source handle, not a consumer lease or a cached Img element.
#[derive(Clone)]
pub(super) struct ArtworkSource {
    sizing: ArtworkImageSizing,
    view: Weak<RefCell<ArtworkViewState>>,
}

impl ArtworkSource {
    fn active(&self) -> bool {
        self.view.upgrade().is_some_and(|state| {
            state
                .borrow()
                .current
                .as_ref()
                .is_some_and(|image| image.consumers.get(&u64::MAX) == Some(&self.sizing.token))
        })
    }

    fn request_cover(&self, window: &mut Window, cx: &mut App) -> bool {
        let active = self.active();
        if active {
            self.sizing.request_cover(window, cx);
        }
        active
    }

    fn load_ambient(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if !self.active() {
            return None;
        }
        self.sizing.image.update(cx, |image, cx| {
            image.artwork.as_ref()?;
            image
                .views
                .insert(self.sizing.token, (window.current_view(), image.generation));
            image.flush_replaced();
            image.load_ambient(self.sizing.token, window, cx)
        })
    }

    pub(super) fn ambient_image(&self) -> Img {
        let source = self.clone();
        img(move |window: &mut Window, cx: &mut App| source.load_ambient(window, cx))
    }

    fn load_static(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if !self.request_cover(window, cx) {
            return None;
        }
        self.sizing.image.update(cx, |image, cx| {
            let path = image.artwork.as_ref()?.path();
            image.load(&Resource::from(path), window, cx)
        })
    }

    pub(super) fn static_image(&self) -> Img {
        let source = self.clone();
        img(move |window: &mut Window, cx: &mut App| source.load_static(window, cx))
    }
}

impl ArtworkView {
    pub(super) fn new(images: ArtworkImageRegistry) -> Self {
        Self {
            state: Rc::default(),
            images,
        }
    }

    pub(super) fn replace(&self, next: Option<LoadedArtwork>, cx: &mut App) -> bool {
        let mut state = self.state.borrow_mut();
        let next = match (state.current.as_mut(), next) {
            (None, None) => return false,
            (Some(current), Some(artwork)) => {
                let same_handle = current.artwork == artwork;
                if same_handle
                    || (current.owner.key.1 == artwork.max_image_bytes()
                        && current.owner.key.0 == artwork.path())
                {
                    // Artwork equality only compares the file handle. Always
                    // adopt the incoming lookup lease, even when it compares equal.
                    current.artwork = artwork;
                    return !same_handle;
                }
                Some(artwork)
            }
            (_, next) => next,
        };
        let next = next.map(|artwork| ArtworkViewImage {
            owner: self.images.acquire(artwork.clone(), cx),
            artwork,
            consumers: HashMap::new(),
        });
        if let Some(old) = std::mem::replace(&mut state.current, next) {
            self.images.retire(old, cx);
        }
        true
    }

    #[cfg(test)]
    pub(super) fn has_artwork(&self) -> bool {
        self.state.borrow().current.is_some()
    }

    #[cfg(test)]
    pub(super) fn decode_failed(&self, cx: &App) -> bool {
        self.state
            .borrow()
            .current
            .as_ref()
            .and_then(|image| {
                image
                    .owner
                    .image
                    .read(cx)
                    .item
                    .as_ref()
                    .and_then(ImageCacheItem::get)
            })
            .is_some_and(|result| result.is_err())
    }

    pub(super) fn sizing(&self, consumer: u64) -> Option<ArtworkImageSizing> {
        let mut state = self.state.borrow_mut();
        let image = state.current.as_mut()?;
        let token = *image.consumers.entry(consumer).or_insert_with(|| {
            let mut registry = self.images.0.borrow_mut();
            registry.next_consumer = registry.next_consumer.wrapping_add(1);
            registry.next_consumer
        });
        Some(ArtworkImageSizing {
            image: image.owner.image.clone(),
            token,
            max_side: None,
        })
    }

    pub(super) fn retain_consumers(&self, visible: &[u64]) {
        let mut state = self.state.borrow_mut();
        let Some(image) = state.current.as_mut() else {
            return;
        };
        let mut retired = Vec::new();
        image.consumers.retain(|consumer, token| {
            if visible.contains(consumer) {
                true
            } else {
                retired.push(*token);
                false
            }
        });
        if !retired.is_empty() {
            self.images
                .0
                .borrow_mut()
                .retired
                .push(RetiredArtworkImage::Demand(
                    image.owner.image.clone(),
                    retired,
                ));
        }
    }

    /// Cache-only readiness for consumers that own animated-image frame pacing.
    /// Loading and failed sources do not start an animation clock. Img's normal
    /// completion notification redraws the consumer when the decode becomes ready.
    pub(super) fn is_animated(&self, cx: &App) -> bool {
        let state = self.state.borrow();
        state.current.as_ref().is_some_and(|image| {
            image
                .owner
                .image
                .read(cx)
                .item
                .as_ref()
                .and_then(ImageCacheItem::get)
                .and_then(Result::ok)
                .is_some_and(|decoded| decoded.frame_count() > 1)
        })
    }

    pub(super) fn source(&self) -> Option<ArtworkSource> {
        // One atmosphere observer belongs to this view's lease, not to each
        // frame-local Img. A ready derivative no longer needs source demand.
        self.sizing(u64::MAX).map(|sizing| ArtworkSource {
            sizing,
            view: Rc::downgrade(&self.state),
        })
    }

    /// Start/use the shared decode and report readiness without copying its pixels.
    /// A transition starts only once its target is renderable, never on I/O start.
    pub(super) fn ready_static_source(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        self.sizing(u64::MAX)?.request_cover(window, cx);
        let state = self.state.borrow();
        let image = state.current.as_ref()?;
        let ready = image.owner.image.update(cx, |cache, cx| {
            cache.load(&Resource::from(image.artwork.path()), window, cx)
        })?;
        Some(ready)
    }

    /// Observe pending/ready work only; an obsolete source must not start cold I/O.
    pub(super) fn cached_static_source(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        // A predecessor may acknowledge retained pixels, but must not create
        // demand or start I/O if it was never decoded.
        self.state
            .borrow()
            .current
            .as_ref()?
            .owner
            .image
            .read(cx)
            .item
            .as_ref()?;
        self.sizing(u64::MAX)?.request_cover(window, cx);
        let state = self.state.borrow();
        let image = state.current.as_ref()?;
        let ready = image
            .owner
            .image
            .read(cx)
            .item
            .as_ref()?
            .use_image(window)?;
        Some(ready)
    }

    pub(super) fn clear_ambient(&self, cx: &mut App) {
        let state = self.state.borrow();
        if let Some(image) = &state.current
            && let Some(decoded) = image.owner.image.update(cx, |image, _| {
                image
                    .ambient
                    .take()
                    .and_then(|item| item.get())
                    .and_then(Result::ok)
            })
        {
            self.images
                .0
                .borrow_mut()
                .retired
                .push(RetiredArtworkImage::Decoded(decoded));
        }
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
            image.release(window, cx);
        }
    }
}

/// Measure the laid-out surface, and keep its placeholder until a frame is ready.
pub(super) fn measured_cover(
    sizing: Option<ArtworkImageSizing>,
    max_side: Option<u32>,
    corner_radius: Pixels,
    fallback: impl FnOnce(Pixels, &Window, &App) -> Img + 'static,
) -> gpui::AnyElement {
    canvas(
        move |bounds, window, cx| {
            let mut image = sizing
                .and_then(|mut sizing| {
                    sizing.max_side = max_side;
                    sizing.request_image(bounds.size.width.max(bounds.size.height), window, cx)
                })
                .unwrap_or_else(|| fallback(bounds.size.width.min(bounds.size.height), window, cx))
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(corner_radius)
                .into_any_element();
            let _ = image.prepaint_as_root(bounds.origin, bounds.size.map(Into::into), window, cx);
            image
        },
        |_, mut image, window, cx| image.paint(window, cx),
    )
    .size_full()
    .into_any_element()
}

struct ArtworkSourceInfo {
    animated: bool,
    width: u32,
    height: u32,
}

impl ArtworkSourceInfo {
    fn longest(&self) -> u32 {
        self.width.max(self.height)
    }
}

struct ReplacedArtworkImage {
    image: Arc<RenderImage>,
    generation: u64,
    // Derived-only observers must not pin obsolete source pixels.
    source_pixels: bool,
}

/// One immutable content path, shared maximum demand, and one in-flight decode.
struct ArtworkImage {
    // None is terminal: scene-held entities cannot reload or protect old files.
    artwork: Option<LoadedArtwork>,
    budget: decode::Budget,
    item: Option<ImageCacheItem>,
    tier: u32,
    target: u32,
    pending: Option<(u32, ImageCacheItem)>,
    source: Arc<OnceLock<ArtworkSourceInfo>>,
    failed_tier: Option<u32>,
    demands: HashMap<u64, u32>,
    views: HashMap<u64, (EntityId, u64)>,
    generation: u64,
    replaced: Vec<ReplacedArtworkImage>,
    downgrade: Option<(u32, Task<()>)>,
    registry: Weak<RefCell<ArtworkImageRegistryState>>,
    ambient: Option<ImageCacheItem>,
}

impl ArtworkImage {
    fn new(
        artwork: LoadedArtwork,
        budget: decode::Budget,
        registry: Weak<RefCell<ArtworkImageRegistryState>>,
        cx: &mut App,
    ) -> Entity<Self> {
        let image = cx.new(|_| Self {
            artwork: Some(artwork),
            budget,
            item: None,
            ambient: None,
            tier: decode::requested_tier(0),
            target: decode::requested_tier(0),
            pending: None,
            source: Arc::default(),
            failed_tier: None,
            demands: HashMap::new(),
            views: HashMap::new(),
            generation: 0,
            replaced: Vec::new(),
            downgrade: None,
            registry,
        });
        cx.observe_release(&image, |image, cx| {
            for decoded in image.take_pixels() {
                cx.drop_image(decoded, None);
            }
        })
        .detach();
        image
    }

    fn take_pixels(&mut self) -> impl Iterator<Item = Arc<RenderImage>> + use<> {
        self.take_guarded_pixels(self.generation)
            .map(|retired| retired.image)
    }

    fn take_guarded_pixels(
        &mut self,
        generation: u64,
    ) -> impl Iterator<Item = ReplacedArtworkImage> + use<> {
        let ambient = self
            .ambient
            .take()
            .and_then(|item| item.get())
            .and_then(Result::ok)
            .map(|image| ReplacedArtworkImage {
                image,
                generation,
                source_pixels: false,
            });
        self.take_source_pixels()
            .map(move |image| ReplacedArtworkImage {
                image,
                generation,
                source_pixels: true,
            })
            .chain(ambient)
            .chain(
                std::mem::take(&mut self.replaced)
                    .into_iter()
                    .map(move |mut retired| {
                        retired.generation = generation;
                        retired
                    }),
            )
    }

    fn take_source_pixels(&mut self) -> impl Iterator<Item = Arc<RenderImage>> + use<> {
        self.downgrade = None;
        self.failed_tier = None;
        [self.item.take(), self.pending.take().map(|(_, item)| item)]
            .into_iter()
            .filter_map(|item| item.and_then(|item| item.get()).and_then(Result::ok))
    }

    fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.artwork = None;
        for decoded in self.take_pixels() {
            cx.drop_image(decoded, Some(window));
        }
    }

    fn cover_side(&self, viewport: gpui::Size<Pixels>) -> Pixels {
        let Some(source) = self.source.get() else {
            // Bootstrap at viewport resolution, then promote once decode has
            // supplied the exact oriented source dimensions.
            return viewport.width.max(viewport.height);
        };
        let longest = source.longest() as f32;
        (viewport.width * (longest / source.width as f32))
            .max(viewport.height * (longest / source.height as f32))
    }

    fn request(
        &mut self,
        consumer: u64,
        side: Pixels,
        max_side: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.artwork.is_none() {
            return;
        }
        let physical = (f32::from(side).max(0.) * window.scale_factor()).ceil() as u32;
        let physical = physical.min(max_side.unwrap_or(u32::MAX));
        let tier = decode::requested_tier(physical);
        if self.demands.insert(consumer, tier) != Some(tier) {
            self.reconcile_demand(window, cx);
        }
    }

    fn reconcile_demand(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.artwork.is_none() {
            return;
        }
        let Some(target) = self.demands.values().copied().max() else {
            // The scene that used these pixels has already crossed a frame edge.
            self.target = decode::requested_tier(0);
            for decoded in self.take_source_pixels() {
                cx.drop_image(decoded, Some(window));
            }
            return;
        };
        self.flush_replaced();
        if self.source.get().is_some_and(|source| source.animated) {
            self.downgrade = None;
            self.target = target;
            return;
        }
        if target >= self.target || self.item.is_none() {
            self.downgrade = None;
            self.target = target;
        } else if self.downgrade.as_ref().map(|(tier, _)| *tier) != Some(target) {
            let timer = cx.background_executor().timer(Duration::from_millis(300));
            self.downgrade = Some((
                target,
                cx.spawn_in(window, async move |this, cx| {
                    timer.await;
                    let _ = this.update_in(cx, |image, window, _| {
                        image.target = target;
                        image.downgrade = None;
                        window.refresh();
                    });
                }),
            ));
        }
    }

    fn decode_item(&self, tier: u32, cx: &mut App) -> ImageCacheItem {
        let artwork = self.artwork.as_ref().expect("live artwork source").clone();
        let budget = self.budget;
        let source = Arc::clone(&self.source);
        if !cx.has_global::<decode::DecodeWorkspace>() {
            cx.set_global(decode::DecodeWorkspace::default());
        }
        let decoder = cx.global::<decode::DecodeWorkspace>().clone();
        ImageCacheItem::from_future(
            async move {
                // Hold the source lease throughout background I/O/decode.
                let mut workspace = decoder.acquire().await;
                let result =
                    workspace.load(&artwork.path(), artwork.max_image_bytes(), budget, tier);
                if let Err(error) = &result {
                    tracing::error!(?error, "artwork_decode_failed");
                }
                result.map(|decoded| {
                    source.get_or_init(|| ArtworkSourceInfo {
                        animated: decoded.animated,
                        width: decoded.source_dimensions.0,
                        height: decoded.source_dimensions.1,
                    });
                    decoded.image
                })
            },
            cx,
        )
    }

    fn invalidate_consumers(&self, cx: &mut App) {
        if self.views.is_empty() {
            return;
        }
        let views: HashSet<_> = self.views.values().map(|(view, _)| *view).collect();
        // Promotion happens inside prepaint. A synchronous notify there marks
        // cached views dirty but does not schedule a frame for their new ACKs.
        cx.defer(move |cx| {
            for view in views {
                cx.notify(view);
            }
        });
    }

    fn flush_replaced(&mut self) {
        if self.replaced.is_empty() {
            return;
        }
        let acknowledged = self
            .views
            .values()
            .map(|(_, generation)| *generation)
            .min()
            .unwrap_or(self.generation);
        let source_acknowledged = self
            .views
            .iter()
            .filter(|(consumer, _)| self.demands.contains_key(*consumer))
            .map(|(_, (_, generation))| *generation)
            .min()
            .unwrap_or(self.generation);
        let mut index = 0;
        while index < self.replaced.len() {
            let retired = &self.replaced[index];
            let acknowledged = if retired.source_pixels {
                source_acknowledged
            } else {
                acknowledged
            };
            if retired.generation <= acknowledged {
                let retired = self.replaced.swap_remove(index);
                self.retire_pixels(retired.image);
            } else {
                index += 1;
            }
        }
    }

    fn retire_pixels(&self, decoded: Arc<RenderImage>) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .borrow_mut()
                .retired
                .push(RetiredArtworkImage::Decoded(decoded));
        }
    }

    fn load_ambient(
        &mut self,
        consumer: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if self.ambient.is_none() {
            self.request(consumer, gpui::px(64.), None, window, cx);
            let source =
                match self.load(&Resource::from(self.artwork.as_ref()?.path()), window, cx)? {
                    Ok(source) => source,
                    Err(error) => return Some(Err(error)),
                };
            self.ambient = Some(ImageCacheItem::from_future(
                async move { Ok(ambient::filtered(&source)) },
                cx,
            ));
        }
        let ready = self.ambient.as_ref()?.use_image(window);
        if ready.is_some() && self.demands.remove(&consumer).is_some() {
            self.flush_replaced();
            if self.demands.is_empty() {
                self.target = decode::requested_tier(0);
                // The derivative owns its pixels. Source-only pixels may still
                // belong to the old scene, so retire them at the normal fence.
                for decoded in self.take_source_pixels() {
                    self.retire_pixels(decoded);
                }
            } else {
                self.reconcile_demand(window, cx);
            }
        }
        ready
    }
}

impl ImageCache for ArtworkImage {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let artwork = self.artwork.as_ref()?;
        debug_assert_eq!(resource, &Resource::from(artwork.path()));
        if self.item.is_none() {
            self.tier = self.target;
            self.item = Some(self.decode_item(self.tier, cx));
        }
        let current = self.item.as_ref()?.use_image(window)?;
        // Animated sources always use their original frames, independent of tier.
        if self.source.get().is_some_and(|source| source.animated) {
            return Some(current);
        }
        let Ok(mut current) = current else {
            return Some(current);
        };
        if let Some((tier, pending)) = &self.pending
            && let Some(result) = pending.use_image(window)
        {
            let tier = *tier;
            let (_, pending) = self.pending.take().expect("completed pending decode");
            let useful = if self.tier < self.target {
                tier > self.tier
            } else {
                tier >= self.target && tier < self.tier
            };
            if let Ok(decoded) = result {
                if useful {
                    self.generation = self.generation.wrapping_add(1);
                    self.item = Some(pending);
                    self.replaced.push(ReplacedArtworkImage {
                        image: std::mem::replace(&mut current, decoded),
                        generation: self.generation,
                        source_pixels: true,
                    });
                    // A cached sibling may never have requested this pending load.
                    // Invalidate every measured consumer, and retain its old atlas
                    // pixels until all consumers have prepainted the new generation.
                    self.invalidate_consumers(cx);
                    self.flush_replaced();
                    self.tier = tier;
                    self.failed_tier = None;
                } else {
                    self.retire_pixels(decoded);
                }
            } else {
                self.failed_tier = Some(tier);
            }
        }
        let size = current.size(0);
        let side = size.width.0.max(size.height.0) as u32;
        let native_satisfies = self
            .source
            .get()
            .is_some_and(|source| source.longest() == side && self.target >= side);
        if self.pending.is_none()
            && !native_satisfies
            && self.tier != self.target
            && self.failed_tier != Some(self.target)
        {
            self.pending = Some((self.target, self.decode_item(self.target, cx)));
            self.pending.as_ref().unwrap().1.use_image(window);
        }
        // Never replace a usable old image with a loading or failed upgrade.
        Some(Ok(current))
    }
}

pub(super) fn owned_artwork_source(source: &OwnedArtworkSource) -> Source<'_> {
    Source {
        id: source.id,
        fingerprint: source.fingerprint.as_deref(),
        path: &source.path,
    }
}

/// One physical executor load; the consumer owns the latest demand.
#[derive(Default)]
pub(super) struct ArtworkLoader {
    active: Option<ArtworkIdentity>,
    negative: Option<ArtworkIdentity>,
}

impl ArtworkLoader {
    /// A visible consumer owns its completed miss independently of cache eviction.
    /// This is demand state, not a lease on a shared cache entry.
    pub(super) fn retain_negative(&mut self, source: Option<Source<'_>>) {
        if !source.is_some_and(|source| {
            self.negative
                .as_ref()
                .is_some_and(|identity| identity.matches(source))
        }) {
            self.negative = None;
        }
    }

    pub(super) fn remember_result(&mut self, identity: ArtworkIdentity, positive: bool) {
        self.negative = (!positive).then_some(identity);
    }

    pub(super) fn prepare(&mut self, source: Source<'_>) -> Option<OwnedArtworkSource> {
        if self.active.is_some()
            || self
                .negative
                .as_ref()
                .is_some_and(|identity| identity.matches(source))
        {
            return None;
        }
        self.active = Some(ArtworkIdentity::new(source));
        Some(OwnedArtworkSource {
            id: source.id,
            fingerprint: source.fingerprint.map(str::to_owned),
            path: source.path.to_owned(),
        })
    }

    pub(super) fn finish(&mut self, identity: &ArtworkIdentity) -> bool {
        if self.active.as_ref() != Some(identity) {
            return false;
        }
        self.active = None;
        true
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DefaultArtworkKey {
    Cover(u32),
    Atmosphere,
}

impl DefaultArtworkKey {
    fn side(self) -> u32 {
        match self {
            Self::Cover(side) => side,
            Self::Atmosphere => ambient::SIDE,
        }
    }
}

#[derive(Default)]
struct DefaultArtworkSizes {
    recent: VecDeque<DefaultArtworkKey>,
    bytes: u64,
}

impl DefaultArtworkSizes {
    fn touch(&mut self, key: DefaultArtworkKey) -> bool {
        if self.recent.back() == Some(&key) {
            return true;
        }
        let Some(index) = self.recent.iter().position(|&cached| cached == key) else {
            return false;
        };
        self.recent.remove(index);
        self.recent.push_back(key);
        true
    }

    fn evict_for(&mut self, key: DefaultArtworkKey) -> Option<DefaultArtworkKey> {
        if self.recent.len() < DEFAULT_ARTWORK_MAX_ENTRIES
            && self.bytes + u64::from(key.side()).pow(2) * 4 <= DEFAULT_ARTWORK_MAX_BYTES
        {
            return None;
        }
        let evicted = self.recent.pop_front()?;
        self.bytes -= u64::from(evicted.side()).pow(2) * 4;
        Some(evicted)
    }

    fn insert(&mut self, key: DefaultArtworkKey) {
        self.bytes += u64::from(key.side()).pow(2) * 4;
        self.recent.push_back(key);
    }
}

struct DefaultArtworkRasterizer {
    renderer: SvgRenderer,
    // The lock is async and acquired only by background asset futures. There is
    // one rasterization at a time, not one worker per card or size.
    parsed: futures::lock::Mutex<Option<ParsedSvg>>,
}

#[derive(Clone)]
struct DefaultArtworkSource {
    key: DefaultArtworkKey,
    rasterizer: Arc<DefaultArtworkRasterizer>,
}

impl Hash for DefaultArtworkSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
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
                        .parse_svg(rivu_presentation::DEFAULT_ARTWORK_SVG)?,
                );
            }
            let side = source.key.side() as i32;
            let image = source.rasterizer.renderer.render_parsed(
                parsed.as_ref().expect("parsed default artwork"),
                SvgSize::ExactSize(gpui::size(
                    gpui::DevicePixels(side),
                    gpui::DevicePixels(side),
                )),
            )?;
            Ok(match source.key {
                DefaultArtworkKey::Cover(_) => image,
                DefaultArtworkKey::Atmosphere => ambient::filtered(&image),
            })
        }
        .await;
        if let Err(error) = &result {
            tracing::error!(?error, key = ?source.key, "default_artwork_raster_failed");
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
            for &key in &cache.sizes.recent {
                let source = cache.source(key);
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

    fn source(&self, key: DefaultArtworkKey) -> DefaultArtworkSource {
        DefaultArtworkSource {
            key,
            rasterizer: Arc::clone(&self.rasterizer),
        }
    }

    fn load(
        &mut self,
        key: DefaultArtworkKey,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        if !self.sizes.touch(key) {
            while let Some(evicted) = self.sizes.evict_for(key) {
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
            self.sizes.insert(key);
        }
        window.use_asset::<DefaultArtworkBitmap>(&self.source(key), cx)
    }
}

/// Square/circular consumers share size-keyed immutable background assets.
#[derive(Clone)]
pub(super) struct DefaultArtwork {
    fallback: gpui::SharedString,
    cache: Entity<DefaultArtworkCache>,
    circular: bool,
}

impl DefaultArtwork {
    pub(super) fn new(i18n: &rivu_presentation::i18n::Localizer, cx: &mut App) -> Self {
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

    pub(super) fn ready(
        &self,
        side: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        let side = default_artwork_side(side, window.scale_factor());
        self.cache.update(cx, |cache, cx| {
            cache.load(DefaultArtworkKey::Cover(side), window, cx)
        })
    }

    #[cfg(test)]
    pub(super) fn assert_composed_cover(&self, cx: &mut App, expected: bool) {
        let cache = self.cache.read(cx);
        if !expected {
            assert!(
                cache.sizes.recent.is_empty(),
                "pending artwork requested fallback"
            );
            return;
        }
        assert_eq!(cache.sizes.recent.len(), 1);
        let key = cache.sizes.recent[0];
        assert!(matches!(key, DefaultArtworkKey::Cover(_)));
        let source = cache.source(key);
        // fetch_asset starts an absent load: check presence before inspecting pixels.
        assert!(cx.has_asset::<DefaultArtworkBitmap>(&source));
        let image = cx
            .fetch_asset::<DefaultArtworkBitmap>(&source)
            .unwrap()
            .unwrap();
        let (pixels, _) = image.as_bytes(0).unwrap().as_chunks::<4>();
        assert!(pixels.iter().any(|pixel| pixel[3] != 0));
    }

    /// A real source owns the surface while loading. Missing or terminally
    /// failed sources use the shared SVG with the same atmosphere processing.
    pub(super) fn background_image(&self, source: Option<ArtworkSource>, ambient: bool) -> Img {
        let fallback = self.clone();
        img(move |window: &mut Window, cx: &mut App| {
            if let Some(source) = &source {
                let result = if ambient {
                    source.load_ambient(window, cx)
                } else {
                    source.load_static(window, cx)
                };
                if !matches!(result, Some(Err(_))) {
                    return result;
                }
            }
            if ambient {
                fallback.cache.update(cx, |cache, cx| {
                    cache.load(DefaultArtworkKey::Atmosphere, window, cx)
                })
            } else {
                let viewport = window.viewport_size();
                fallback.ready(viewport.width.max(viewport.height), window, cx)
            }
        })
    }

    pub(super) fn image(
        &self,
        theme: &'static rivu_format::palette::Palette,
        side: Pixels,
        window: &Window,
    ) -> Img {
        let side = default_artwork_side(side, window.scale_factor());
        let cache = self.cache.clone();
        let fallback = self.fallback.clone();
        img(move |window: &mut Window, cx: &mut App| {
            cache.update(cx, |cache, cx| {
                cache.load(DefaultArtworkKey::Cover(side), window, cx)
            })
        })
        .with_fallback(move || rivu_ui::caption(theme, fallback.clone()).into_any_element())
    }
}

impl Render for DefaultArtwork {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = rivu_ui::theme::palette(cx);
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
                        let artwork = DefaultArtwork {
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

    const VINYL: &[u8] = include_bytes!("../../data/artwork/vinyl.svg");

    fn cover_wave(path: &Path, image: &[u8]) {
        rivu_metadata::artwork::test_support::pictures_wav(path, 3, &[(image, image.len(), 3)]);
    }

    fn loaded_cover(bytes: &[u8]) -> (tempfile::TempDir, LoadedArtwork) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        cover_wave(&path, bytes);
        let manager = rivu_metadata::artwork::ArtworkManager::new(directory.path());
        let loaded = manager
            .load(crate::artwork_source(
                &rivu_protocol::test_support::artwork_track(path),
            ))
            .unwrap();
        (directory, loaded)
    }

    fn install_cover(
        view: &ArtworkView,
        loaded: LoadedArtwork,
        cx: &mut App,
    ) -> Entity<ArtworkImage> {
        view.replace(Some(loaded), cx);
        view.state
            .borrow()
            .current
            .as_ref()
            .unwrap()
            .owner
            .image
            .clone()
    }

    #[gpui::test]
    fn same_image_swap_keeps_decode_and_measured_consumer(cx: &mut gpui::TestAppContext) {
        struct CoverScene(ArtworkImageSizing, bool);

        impl Render for CoverScene {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let sizing = self.0.clone();
                let expected_ready = self.1;
                canvas(
                    move |bounds, window, cx| {
                        assert_eq!(
                            sizing
                                .request_image(bounds.size.width, window, cx)
                                .is_some(),
                            expected_ready
                        );
                    },
                    |_, (), _, _| {},
                )
                .size_full()
            }
        }

        fn draw(window: &mut gpui::VisualTestContext, sizing: ArtworkImageSizing, ready: bool) {
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(128.), gpui::px(128.)),
                |_, cx| cx.new(|_| CoverScene(sizing, ready)).into_any_element(),
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        cover_wave(&path, &decode::tests::gif());
        let manager = rivu_metadata::artwork::ArtworkManager::new(directory.path());
        let mut track = rivu_protocol::test_support::artwork_track(path);
        track.id = 1;
        let first = manager.load(crate::artwork_source(&track)).unwrap();
        let mut alias = track.clone();
        alias.id = 2;
        let latest = manager.load(crate::artwork_source(&alias)).unwrap();
        assert_eq!(first, latest, "aliases share pixels but not lookup leases");

        let images = ArtworkImageRegistry::new(2);
        let view = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let (cache, sizing) = window.update(|_, cx| {
            let cache = install_cover(&view, first, cx);
            (cache, view.sizing(1).unwrap())
        });
        draw(window, sizing.clone(), false);
        window.run_until_parked();
        draw(window, sizing.clone(), true);
        let (next, decoded) = window.update(|_, cx| {
            let decoded = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert!(decoded.frame_count() > 1);

            view.replace(Some(latest), cx);
            let next = view.sizing(1).unwrap();
            assert_eq!(next.token, sizing.token);
            assert_eq!(next.image.entity_id(), cache.entity_id());
            (next, decoded)
        });
        // Only the view now holds the selected alias. Churn idle logical entries
        // that share the file to distinguish its lease from the old track's.
        for id in 3..=130 {
            track.id = id;
            drop(manager.load(crate::artwork_source(&track)).unwrap());
        }
        track.id = 3;
        assert!(
            manager
                .cached_result(crate::artwork_source(&track))
                .is_none()
        );
        assert!(manager.cached(crate::artwork_source(&alias)).is_some());

        draw(window, next, true);
        window.update(|window, cx| {
            let retained = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&decoded, &retained));

            view.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    // Each test selects distinct pixels so global same-source decode sharing
    // cannot make another concurrently running test own its release probe.
    fn static_cover(side: u32, red: u8) -> Vec<u8> {
        rectangular_cover(side, side / 2, red)
    }

    fn rectangular_cover(width: u32, height: u32, red: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([red, 90, 110, 128]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    struct EmptyMaterialScene;

    impl Render for EmptyMaterialScene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    struct StaticMaterialScene {
        artwork: ArtworkView,
        direct: bool,
    }

    impl Render for StaticMaterialScene {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let image = if self.direct {
                self.artwork
                    .ready_static_source(window, cx)
                    .and_then(Result::ok)
                    .map(img)
            } else {
                self.artwork.source().map(|source| source.static_image())
            };
            div()
                .size_full()
                .children(image.map(|image| image.size_full().object_fit(gpui::ObjectFit::Cover)))
        }
    }

    fn draw_static_material(
        window: &mut gpui::VisualTestContext,
        artwork: &ArtworkView,
        direct: bool,
    ) {
        let viewport = window.update(|window, _| window.viewport_size());
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            viewport,
            |_, cx| {
                cx.new(|_| StaticMaterialScene {
                    artwork: artwork.clone(),
                    direct,
                })
                .into_any_element()
            },
        );
    }

    fn exercise_material_cover(
        cx: &mut gpui::TestAppContext,
        dimensions: (u32, u32),
        viewport: (f32, f32),
        expected_tier: u32,
        direct: bool,
        red: u8,
    ) {
        let (_directory, loaded) =
            loaded_cover(&rectangular_cover(dimensions.0, dimensions.1, red));
        let images = ArtworkImageRegistry::default();
        let material = ArtworkView::new(images.clone());
        let sibling = ArtworkView::new(images.clone());
        let handle = cx.open_window(
            gpui::size(gpui::px(viewport.0), gpui::px(viewport.1)),
            |_, _| EmptyMaterialScene,
        );
        cx.simulate_window_scale_factor_change(handle.into(), 1.);
        let window = &mut gpui::VisualTestContext::from_window(handle.into(), cx);
        let source = window.update(|_, cx| {
            sibling.replace(Some(loaded.clone()), cx);
            material.replace(Some(loaded), cx);
            material.source().unwrap()
        });
        let cache = source.sizing.image.clone();
        request_cover_size(window, &sibling, 1, 200.);
        window.update(|window, cx| {
            assert_eq!(window.scale_factor(), 1.);
            assert!(cache.read(cx).source.get().is_none());
            assert!(cache.read(cx).item.is_none());
            assert_eq!(sibling.sizing(1).unwrap().image, cache);
        });

        // Both material routes bootstrap without knowing the source aspect.
        let initial_tier = decode::requested_tier(viewport.0.max(viewport.1).ceil() as u32);
        draw_static_material(window, &material, direct);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.target, initial_tier);
            assert_eq!(image.tier, initial_tier);
            assert_eq!(image.demands.len(), 2);
            assert!(image.pending.is_none());
        });
        window.run_until_parked();
        window.update(|_, cx| {
            let image = cache.read(cx);
            let source = image.source.get().unwrap();
            assert_eq!((source.width, source.height), dimensions);
            assert!(image.item.as_ref().unwrap().get().unwrap().is_ok());
        });

        // The first ready frame re-evaluates Cover, using exact source metadata
        // rather than the rounded dimensions of the bootstrap thumbnail.
        draw_static_material(window, &material, direct);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.target, expected_tier);
            if expected_tier != initial_tier {
                assert_eq!(
                    image.pending.as_ref().map(|(tier, _)| *tier),
                    Some(expected_tier)
                );
            }
        });
        window.run_until_parked();
        draw_static_material(window, &material, direct);
        let pixels = window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.tier, expected_tier);
            assert!(image.pending.is_none());
            assert!(image.downgrade.is_none());
            let pixels = image.item.as_ref().unwrap().get().unwrap().unwrap();
            let expected_size = if expected_tier == u32::MAX {
                dimensions
            } else {
                (expected_tier, expected_tier)
            };
            assert_eq!(
                pixels.size(0),
                gpui::size(
                    gpui::DevicePixels(expected_size.0 as i32),
                    gpui::DevicePixels(expected_size.1 as i32),
                )
            );
            pixels
        });
        for _ in 0..3 {
            draw_static_material(window, &material, direct);
            window.run_until_parked();
            window.update(|_, cx| {
                let image = cache.read(cx);
                assert_eq!(image.target, expected_tier);
                assert_eq!(image.tier, expected_tier);
                assert_eq!(image.generation, u64::from(initial_tier != expected_tier));
                assert!(image.pending.is_none());
                assert!(image.downgrade.is_none());
                assert!(Arc::ptr_eq(
                    &pixels,
                    &image.item.as_ref().unwrap().get().unwrap().unwrap(),
                ));
            });
        }
        drop(pixels);

        // Retiring this route cannot rejoin a still-live sibling's shared lease.
        window.update(|window, cx| {
            material.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(cache.read(cx).artwork.is_some());
            assert_eq!(cache.read(cx).demands.len(), 1);
            assert!(material.ready_static_source(window, cx).is_none());
        });
        draw_source(window, source.static_image());
        window.update(|window, cx| {
            assert_eq!(cache.read(cx).demands.len(), 1);
            assert!(cache.read(cx).pending.is_none());
            sibling.release_all_images(window, cx);
            images.release_all_retired(window, cx);
            assert!(cache.read(cx).artwork.is_none());
            assert!(cache.read(cx).item.is_none());
        });
        draw_source(window, source.static_image());
        window.run_until_parked();
        window.update(|_, cx| {
            assert!(cache.read(cx).item.is_none());
            assert!(cache.read(cx).pending.is_none());
            assert!(cache.read(cx).demands.is_empty());
        });
    }

    #[gpui::test]
    fn material_cover_promotes_portrait_landscape_and_exact_aspect_boundary(
        cx: &mut gpui::TestAppContext,
    ) {
        for direct in [false, true] {
            for (index, (dimensions, viewport)) in [
                ((1000, 4000), (800., 600.)),
                ((4000, 1000), (800., 600.)),
                // At 256px, 4096x1023 rounds to 256x64. That rounded ratio
                // would incorrectly keep 1024px instead of the original tier.
                ((4096, 1023), (256., 256.)),
            ]
            .into_iter()
            .enumerate()
            {
                exercise_material_cover(
                    cx,
                    dimensions,
                    viewport,
                    u32::MAX,
                    direct,
                    130 + index as u8 + 3 * u8::from(direct),
                );
            }
        }
    }

    #[gpui::test]
    fn material_cover_keeps_square_and_large_viewports_at_their_existing_tier(
        cx: &mut gpui::TestAppContext,
    ) {
        for direct in [false, true] {
            exercise_material_cover(
                cx,
                (1600, 1600),
                (800., 600.),
                1024,
                direct,
                136 + u8::from(direct),
            );
            exercise_material_cover(
                cx,
                (1000, 4000),
                (1600., 1200.),
                u32::MAX,
                direct,
                138 + u8::from(direct),
            );
        }
    }

    #[gpui::test]
    fn retired_unknown_material_cover_sources_cannot_start_shared_or_released_decodes(
        cx: &mut gpui::TestAppContext,
    ) {
        for shared in [false, true] {
            let (_directory, loaded) =
                loaded_cover(&rectangular_cover(1000, 4000, 140 + u8::from(shared)));
            let images = ArtworkImageRegistry::default();
            let material = ArtworkView::new(images.clone());
            let sibling = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let source = window.update(|window, cx| {
                if shared {
                    sibling.replace(Some(loaded.clone()), cx);
                }
                material.replace(Some(loaded), cx);
                let source = material.source().unwrap();
                material.replace(None, cx);
                images.release_retired(window);
                window.simulate_next_frame(cx);
                assert!(material.ready_static_source(window, cx).is_none());
                source
            });
            draw_source(window, source.static_image());
            window.run_until_parked();
            window.update(|window, cx| {
                let image = source.sizing.image.read(cx);
                assert_eq!(image.artwork.is_some(), shared);
                assert!(image.source.get().is_none());
                assert!(image.item.is_none());
                assert!(image.pending.is_none());
                assert!(image.demands.is_empty());
                sibling.release_all_images(window, cx);
                images.release_all_retired(window, cx);
            });
        }
    }

    #[gpui::test]
    fn album_thumbnail_limit_preserves_independent_large_image_demand(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 74));
        let images = ArtworkImageRegistry::default();
        let cover = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        window.update(|_, cx| cover.replace(Some(loaded), cx));
        let mut album = cover.sizing(1).unwrap();
        album.max_side = Some(560);
        draw_sizing(window, album, 2000.);
        window.run_until_parked();
        window.update(|_, cx| {
            let state = cover.state.borrow();
            let image = state.current.as_ref().unwrap().owner.image.read(cx);
            let pixels = image.item.as_ref().unwrap().get().unwrap().unwrap();
            assert_eq!(pixels.size(0).width.0, 560);
            assert_eq!(pixels.size(0).height.0, 280);
        });
        draw_sizing(window, cover.sizing(2).unwrap(), 800.);
        window.run_until_parked();
        draw_sizing(window, cover.sizing(2).unwrap(), 800.);
        window.update(|window, cx| {
            {
                let state = cover.state.borrow();
                let image = state.current.as_ref().unwrap().owner.image.read(cx);
                let pixels = image.item.as_ref().unwrap().get().unwrap().unwrap();
                assert_eq!(pixels.size(0).width.0, 1024);
            }
            cover.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    fn request_cover_size(
        window: &mut gpui::VisualTestContext,
        cover: &ArtworkView,
        consumer: u64,
        physical_side: f32,
    ) {
        window.update(|window, cx| {
            let sizing = cover.sizing(consumer).unwrap();
            sizing.image.update(cx, |image, cx| {
                image.request(
                    sizing.token,
                    gpui::px(physical_side / window.scale_factor()),
                    None,
                    window,
                    cx,
                );
            });
        });
    }

    #[gpui::test]
    fn material_source_keeps_viewport_demand_after_the_artwork_panel_retires(
        cx: &mut gpui::TestAppContext,
    ) {
        struct MaterialScene(ArtworkView);

        impl Render for MaterialScene {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().size_full().children(
                    self.0
                        .ready_static_source(window, cx)
                        .and_then(Result::ok)
                        .map(img),
                )
            }
        }

        fn draw_material(window: &mut gpui::VisualTestContext, material: &ArtworkView) {
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(640.), gpui::px(480.)),
                |_, cx| {
                    cx.new(|_| MaterialScene(material.clone()))
                        .into_any_element()
                },
            );
        }

        let (_directory, loaded) = loaded_cover(&static_cover(2048, 83));
        let images = ArtworkImageRegistry::default();
        let material = ArtworkView::new(images.clone());
        let panel = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            material.replace(Some(loaded.clone()), cx);
            panel.replace(Some(loaded.clone()), cx);
            material
                .state
                .borrow()
                .current
                .as_ref()
                .unwrap()
                .owner
                .image
                .clone()
        });
        draw_material(window, &material);
        window.update(|window, cx| {
            let viewport = window.viewport_size();
            let expected = decode::requested_tier(
                (f32::from(viewport.width.max(viewport.height)) * window.scale_factor()).ceil()
                    as u32,
            );
            assert_eq!(cache.read(cx).target, expected);
            assert_eq!(cache.read(cx).demands.len(), 1);
        });
        window.run_until_parked();
        draw_material(window, &material);
        window.update(|_, cx| {
            assert!(cache.read(cx).item.as_ref().unwrap().get().unwrap().is_ok());
        });
        request_cover_size(window, &panel, 1, 800.);
        window.update(|window, cx| {
            assert_eq!(cache.read(cx).demands.len(), 2);
            let retired_source = material.source().unwrap();
            material.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert_eq!(cache.read(cx).demands.len(), 1);
            assert!(!retired_source.request_cover(window, cx));
            assert_eq!(cache.read(cx).demands.len(), 1);
            material.replace(Some(loaded.clone()), cx);
        });
        draw_material(window, &material);
        window.update(|window, cx| {
            assert_eq!(cache.read(cx).demands.len(), 2);
            panel.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert_eq!(cache.read(cx).demands.len(), 1);
            assert!(cache.read(cx).item.is_some());
            assert!(cache.read(cx).artwork.is_some());
            material.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(cache.read(cx).artwork.is_none());
            assert!(cache.read(cx).item.is_none());
        });
    }

    fn draw_ambient(window: &mut gpui::VisualTestContext, material: &ArtworkView) {
        draw_source(window, material.source().unwrap().ambient_image());
    }

    fn draw_source(window: &mut gpui::VisualTestContext, image: Img) {
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(64.), gpui::px(64.)),
            |_, cx| cx.new(|_| DecorationScene(Some(image))).into_any_element(),
        );
    }

    #[gpui::test]
    fn ambient_ready_retires_source_at_frame_edge_and_reuses_derivative(
        cx: &mut gpui::TestAppContext,
    ) {
        for immediate_release in [false, true] {
            let (_directory, loaded) =
                loaded_cover(&static_cover(2048, 229 + u8::from(immediate_release)));
            let images = ArtworkImageRegistry::default();
            let material = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let cache = window.update(|_, cx| {
                material.replace(Some(loaded), cx);
                material.source().unwrap().sizing.image
            });
            draw_ambient(window, &material);
            window.run_until_parked();
            let source = window.update(|_, cx| {
                let image = cache.read(cx);
                assert_eq!(image.demands.len(), 1);
                Arc::downgrade(&image.item.as_ref().unwrap().get().unwrap().unwrap())
            });
            draw_ambient(window, &material);
            window.run_until_parked();
            draw_ambient(window, &material);
            let ambient = window.update(|window, cx| {
                let image = cache.read(cx);
                assert!(image.demands.is_empty());
                assert_eq!(
                    image.views.len(),
                    1,
                    "derived consumers still observe invalidation"
                );
                assert!(image.item.is_none());
                assert!(image.pending.is_none());
                assert!(image.downgrade.is_none());
                assert!(
                    source.upgrade().is_some(),
                    "old scene has not crossed its fence"
                );
                let ambient = image.ambient.as_ref().unwrap().get().unwrap().unwrap();
                assert_eq!(
                    ambient.size(0),
                    gpui::size(gpui::DevicePixels(64), gpui::DevicePixels(64))
                );
                images.release_retired(window);
                window.simulate_next_frame(cx);
                assert!(source.upgrade().is_none());
                ambient
            });
            for _ in 0..3 {
                draw_ambient(window, &material);
                window.run_until_parked();
                window.update(|_, cx| {
                    let image = cache.read(cx);
                    assert!(image.demands.is_empty());
                    assert!(image.item.is_none());
                    assert!(Arc::ptr_eq(
                        &ambient,
                        &image.ambient.as_ref().unwrap().get().unwrap().unwrap()
                    ));
                });
            }
            let ambient_pixels = Arc::downgrade(&ambient);
            drop(ambient);
            let old_scene = material.source().unwrap().ambient_image();
            window.update(|window, cx| {
                if immediate_release {
                    material.release_all_images(window, cx);
                    images.release_all_retired(window, cx);
                } else {
                    material.replace(None, cx);
                    images.release_retired(window);
                    window.simulate_next_frame(cx);
                }
                assert!(cache.read(cx).artwork.is_none());
                assert!(cache.read(cx).views.is_empty());
                assert!(cache.read(cx).ambient.is_none());
                assert!(ambient_pixels.upgrade().is_none());
            });
            draw_source(window, old_scene);
            window.run_until_parked();
            window.update(|_, cx| {
                assert!(cache.read(cx).item.is_none());
                assert!(cache.read(cx).ambient.is_none());
            });
        }
    }

    #[gpui::test]
    fn ambient_preserves_other_source_consumers_and_source_requests_can_reload(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 231));
        let images = ArtworkImageRegistry::default();
        let material = ArtworkView::new(images.clone());
        let panel = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            material.replace(Some(loaded.clone()), cx);
            panel.replace(Some(loaded.clone()), cx);
            material.source().unwrap().sizing.image
        });
        draw_sizing(window, panel.sizing(1).unwrap(), 800.);
        window.run_until_parked();
        draw_ambient(window, &material);
        window.run_until_parked();
        draw_ambient(window, &material);
        let replaced_source = window.update(|_, cx| {
            Arc::downgrade(
                &cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .unwrap(),
            )
        });
        // A cached derivative does not need to render/ACK a source-only upgrade.
        draw_sizing(window, panel.sizing(1).unwrap(), 1100.);
        window.run_until_parked();
        draw_sizing(window, panel.sizing(1).unwrap(), 1100.);
        // The promotion frame still registered the old generation. Repaint the
        // panel to acknowledge the new pixels before fencing their predecessor.
        draw_sizing(window, panel.sizing(1).unwrap(), 1100.);
        let (source, ambient) = window.update(|window, cx| {
            let image = cache.read(cx);
            assert_eq!(
                image.demands.len(),
                1,
                "only the panel retains source demand"
            );
            assert_eq!(image.target, decode::requested_tier(1100));
            assert_eq!(image.tier, image.target);
            assert!(image.replaced.is_empty());
            assert!(image.downgrade.is_none());
            let source = Arc::downgrade(&image.item.as_ref().unwrap().get().unwrap().unwrap());
            let ambient = image.ambient.as_ref().unwrap().get().unwrap().unwrap();
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(
                replaced_source.upgrade().is_none(),
                "ambient observers must not pin retired source generations"
            );
            assert!(
                source.upgrade().is_some(),
                "the panel still needs these pixels"
            );
            panel.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(cache.read(cx).item.is_none());
            assert!(source.upgrade().is_none());
            assert!(cache.read(cx).ambient.is_some());
            (source, ambient)
        });
        // Switching this live material back to a source consumer reloads at its
        // actual viewport demand, not at the former ambient bootstrap size.
        draw_source(window, material.source().unwrap().static_image());
        window.run_until_parked();
        draw_source(window, material.source().unwrap().static_image());
        window.update(|window, cx| {
            let image = cache.read(cx);
            let viewport = window.viewport_size();
            assert_eq!(
                image.target,
                decode::requested_tier(
                    (f32::from(viewport.width.max(viewport.height * 2.)) * window.scale_factor())
                        .ceil() as u32
                )
            );
            assert_eq!(image.tier, image.target);
            assert!(image.item.as_ref().unwrap().get().unwrap().is_ok());
            assert!(source.upgrade().is_none());
            assert!(Arc::ptr_eq(
                &ambient,
                &image.ambient.as_ref().unwrap().get().unwrap().unwrap()
            ));
            panel.replace(Some(loaded), cx);
        });
        request_cover_size(window, &panel, 1, 800.);
        let stale = material.source().unwrap().ambient_image();
        window.update(|window, cx| {
            material.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(
                cache.read(cx).artwork.is_some(),
                "another consumer owns the source"
            );
            assert_eq!(cache.read(cx).demands.len(), 1);
        });
        draw_source(window, stale);
        window.update(|window, cx| {
            assert_eq!(
                cache.read(cx).demands.len(),
                1,
                "stale handles cannot rejoin a live source"
            );
            panel.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn ambient_only_budget_changes_invalidate_derivative_without_losing_observer(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 232));
        let images = ArtworkImageRegistry::default();
        let material = ArtworkView::new(images.clone());
        let sibling = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            sibling.replace(Some(loaded.clone()), cx);
            material.replace(Some(loaded), cx);
            material.source().unwrap().sizing.image
        });
        draw_ambient(window, &material);
        window.run_until_parked();
        draw_ambient(window, &material);
        window.run_until_parked();
        draw_ambient(window, &material);
        draw_ambient(window, &sibling);
        let old_ambient = window.update(|window, cx| {
            images.release_retired(window);
            window.simulate_next_frame(cx);
            let old = Arc::downgrade(
                &cache
                    .read(cx)
                    .ambient
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .unwrap(),
            );
            assert!(cache.read(cx).demands.is_empty());
            assert!(images.set_decode_budget_mib(16, cx));
            assert!(cache.read(cx).ambient.is_none());
            assert!(cache.read(cx).item.is_none());
            assert_eq!(cache.read(cx).views.len(), 2);
            assert!(
                old.upgrade().is_some(),
                "budget invalidation retains the old scene until ACK"
            );
            old
        });
        draw_ambient(window, &material);
        window.run_until_parked();
        draw_ambient(window, &material);
        window.run_until_parked();
        draw_ambient(window, &material);
        window.update(|window, cx| {
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(
                old_ambient.upgrade().is_some(),
                "the cached sibling has not ACKed the new generation"
            );
            assert_eq!(cache.read(cx).replaced.len(), 1);
            sibling.replace(None, cx);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            // Retiring an ambient-only observer removes no source demand, but
            // still acknowledges guarded replacements at the same fence.
            images.release_all_retired(window, cx);
            assert!(old_ambient.upgrade().is_none());
            let image = cache.read(cx);
            assert!(image.ambient.as_ref().unwrap().get().unwrap().is_ok());
            assert!(image.demands.is_empty());
            assert!(image.item.is_none());
            assert!(image.pending.is_none());
            assert!(image.replaced.is_empty());
            material.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_static_upgrade_notifies_mounted_cached_consumers_after_prepaint(
        cx: &mut gpui::TestAppContext,
    ) {
        struct Panel {
            cover: ArtworkView,
            consumer: u64,
            renders: Rc<std::cell::Cell<usize>>,
            fallback: Arc<RenderImage>,
        }
        impl Render for Panel {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                let fallback = Arc::clone(&self.fallback);
                measured_cover(
                    self.cover.sizing(self.consumer),
                    None,
                    gpui::px(0.),
                    move |_, _, _| img(fallback),
                )
            }
        }
        struct Root {
            first: Entity<Panel>,
            second: Entity<Panel>,
            side: Pixels,
            images: ArtworkImageRegistry,
        }
        impl Render for Root {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let images = self.images.clone();
                div()
                    .relative()
                    .flex()
                    .size_full()
                    .child(
                        div().size(gpui::px(64.)).child(
                            self.first
                                .clone()
                                .cached(gpui::StyleRefinement::default().size_full()),
                        ),
                    )
                    .child(
                        div().size(self.side).child(
                            self.second
                                .clone()
                                .cached(gpui::StyleRefinement::default().size_full()),
                        ),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _| images.release_retired(window),
                        )
                        .absolute()
                        .size_full(),
                    )
            }
        }
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 71));
        let images = ArtworkImageRegistry::default();
        let view = ArtworkView::new(images.clone());
        let cache = cx.update(|cx| {
            view.replace(Some(loaded), cx);
            view.state
                .borrow()
                .current
                .as_ref()
                .unwrap()
                .owner
                .image
                .clone()
        });
        let first_renders = Rc::new(std::cell::Cell::new(0));
        let second_renders = Rc::new(std::cell::Cell::new(0));
        let fallback = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        let window = cx.open_window(gpui::size(gpui::px(640.), gpui::px(480.)), {
            let view = view.clone();
            let images = images.clone();
            let first_renders = Rc::clone(&first_renders);
            let second_renders = Rc::clone(&second_renders);
            move |_, cx| Root {
                first: cx.new(|_| Panel {
                    cover: view.clone(),
                    consumer: 1,
                    renders: first_renders,
                    fallback: Arc::clone(&fallback),
                }),
                second: cx.new(|_| Panel {
                    cover: view,
                    consumer: 2,
                    renders: second_renders,
                    fallback,
                }),
                side: gpui::px(64.),
                images,
            }
        });
        let redraw = Rc::new(std::cell::Cell::new(true));
        let panels = window
            .read_with(cx, |root, _| [root.first.clone(), root.second.clone()])
            .unwrap();
        cx.update(|cx| {
            for panel in &panels {
                let redraw = Rc::clone(&redraw);
                cx.observe(panel, move |_, _| redraw.set(true)).detach();
            }
        });
        let settle_notified_frames = |cx: &mut gpui::TestAppContext| {
            for _ in 0..16 {
                cx.run_until_parked();
                let callbacks = cx
                    .update_window(window.into(), |_, window, cx| {
                        window.simulate_next_frame(cx)
                    })
                    .unwrap();
                if !redraw.replace(false) && callbacks == 0 {
                    return;
                }
                cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                    .unwrap();
            }
            panic!("static cached artwork must finish its notified redraws");
        };
        settle_notified_frames(cx);
        let (old_pixels, first_before, second_before) = cx.update(|cx| {
            assert_eq!(cache.read(cx).tier, 200);
            assert_eq!(cache.read(cx).views.len(), 2);
            (
                Arc::downgrade(
                    &cache
                        .read(cx)
                        .item
                        .as_ref()
                        .unwrap()
                        .get()
                        .unwrap()
                        .unwrap(),
                ),
                first_renders.get(),
                second_renders.get(),
            )
        });
        window
            .update(cx, |root, _, cx| {
                root.side = gpui::px(200.);
                cx.notify();
            })
            .unwrap();
        // TestWindow's platform frame methods are private to GPUI. Drive this
        // mounted test only from public Notify effects or explicit frame
        // callbacks. An in-prepaint synchronous notify emits neither effect.
        redraw.set(true); // The one explicit size-change event above.
        settle_notified_frames(cx);
        cx.update(|cx| {
            let image = cache.read(cx);
            assert_eq!(image.tier, 400);
            assert!(image.replaced.is_empty());
            assert!(
                image
                    .views
                    .values()
                    .all(|(_, generation)| *generation == image.generation)
            );
            assert!(old_pixels.upgrade().is_none());
            assert!(
                first_renders.get() > first_before,
                "unchanged cached sibling must redraw"
            );
            assert!(second_renders.get() > second_before);
        });
        assert!(
            !redraw.get(),
            "static artwork must finish its notified redraws"
        );
        let (budget_pixels, first_before, second_before) = cx.update(|cx| {
            (
                Arc::downgrade(
                    &cache
                        .read(cx)
                        .item
                        .as_ref()
                        .unwrap()
                        .get()
                        .unwrap()
                        .unwrap(),
                ),
                first_renders.get(),
                second_renders.get(),
            )
        });
        cx.update(|cx| {
            assert!(images.set_decode_budget_mib(128, cx));
            assert!(cache.read(cx).item.is_none());
            assert!(!cache.read(cx).replaced.is_empty());
        });
        settle_notified_frames(cx);
        let final_pixels = cx.update(|cx| {
            let image = cache.read(cx);
            assert!(
                budget_pixels.upgrade().is_none(),
                "budget-reset scenes release their previous pixels"
            );
            assert!(first_renders.get() > first_before);
            assert!(second_renders.get() > second_before);
            assert!(image.replaced.is_empty());
            assert!(
                image
                    .views
                    .values()
                    .all(|(_, generation)| *generation == image.generation)
            );
            Arc::downgrade(&image.item.as_ref().unwrap().get().unwrap().unwrap())
        });
        assert!(
            !redraw.get(),
            "budget redraws must also finish without unrelated events"
        );
        cx.update_window(window.into(), |_, window, cx| {
            view.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                final_pixels.upgrade().is_none(),
                "closing all consumers releases the final decode"
            );
            assert!(cache.read(cx).item.is_none());
            assert!(cache.read(cx).replaced.is_empty());
        });
    }

    #[gpui::test]
    fn artwork_superseded_downgrade_and_failed_upgrade_keep_suitable_pixels(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 72));
        let images = ArtworkImageRegistry::default();
        let view = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| install_cover(&view, loaded, cx));
        draw_sizing(window, view.sizing(1).unwrap(), 800.);
        window.run_until_parked();
        let original = window.update(|_, cx| {
            cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap()
        });
        // A 256 downgrade finishes after the user grows back to 560. The
        // current 1024 is still suitable; publishing 256 would visibly regress.
        window.update(|_, cx| {
            cache.update(cx, |image, cx| {
                image.target = 560;
                image.pending = Some((
                    256,
                    ImageCacheItem::from_future(
                        async {
                            Ok(Arc::new(RenderImage::new([image::Frame::new(
                                image::RgbaImage::new(256, 128),
                            )])))
                        },
                        cx,
                    ),
                ));
            });
        });
        window.run_until_parked();
        draw_sizing(window, view.sizing(1).unwrap(), 560.);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.tier, 1024);
            assert_eq!(image.pending.as_ref().unwrap().0, 560);
            assert!(Arc::ptr_eq(
                &original,
                &image.item.as_ref().unwrap().get().unwrap().unwrap()
            ));
        });
        // A failed replacement is terminal for that target, not a render loop
        // of retries, and must not discard the valid current image.
        window.update(|_, cx| {
            cache.update(cx, |image, cx| {
                image.pending = Some((
                    560,
                    ImageCacheItem::from_future(
                        async { Err(ImageCacheError::Asset("controlled decode failure".into())) },
                        cx,
                    ),
                ));
            });
        });
        window.run_until_parked();
        for _ in 0..3 {
            draw_sizing(window, view.sizing(1).unwrap(), 560.);
            window.update(|_, cx| {
                let image = cache.read(cx);
                assert!(image.pending.is_none());
                assert_eq!(image.failed_tier, Some(560));
                assert!(Arc::ptr_eq(
                    &original,
                    &image.item.as_ref().unwrap().get().unwrap().unwrap()
                ));
            });
        }
        window.update(|window, cx| {
            view.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_thumbnail_demands_share_upgrades_and_debounce_retired_large_consumers(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&static_cover(2048, 73));
        let images = ArtworkImageRegistry::default();
        let first = ArtworkView::new(images.clone());
        let second = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            let cache = install_cover(&first, loaded.clone(), cx);
            second.replace(Some(loaded), cx);
            cache
        });
        request_cover_size(window, &second, 2, 201.);
        draw_sizing(window, first.sizing(1).unwrap(), 256.);
        window.run_until_parked();
        let original = window.update(|_, cx| {
            let image = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert_eq!(image.size(0).width.0, 256);
            image
        });
        draw_sizing(window, first.sizing(1).unwrap(), 257.);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.target, 400);
            assert_eq!(image.pending.as_ref().unwrap().0, 400);
            assert!(Arc::ptr_eq(
                &original,
                &image.item.as_ref().unwrap().get().unwrap().unwrap()
            ));
        });
        request_cover_size(window, &first, 1, 800.);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.target, 1024);
            assert_eq!(
                image.pending.as_ref().unwrap().0,
                400,
                "one in-flight decode"
            );
        });
        window.run_until_parked();
        draw_sizing(window, first.sizing(1).unwrap(), 800.);
        window.run_until_parked();
        draw_sizing(window, first.sizing(1).unwrap(), 800.);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.tier, 1024);
            assert!(image.pending.is_none());
            assert_eq!(
                image
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .unwrap()
                    .size(0)
                    .width
                    .0,
                1024
            );
        });
        // Per-pixel changes within the same tier do not start another decode.
        draw_sizing(window, first.sizing(1).unwrap(), 801.);
        window.update(|window, cx| {
            assert!(cache.read(cx).pending.is_none());
            first.retain_consumers(&[]);
            assert_eq!(
                cache.read(cx).demands.len(),
                2,
                "old scene still owns demand"
            );
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert_eq!(cache.read(cx).demands.len(), 1);
            assert_eq!(cache.read(cx).target, 1024);
            assert_eq!(cache.read(cx).downgrade.as_ref().unwrap().0, 256);
        });
        window.run_until_parked();
        window.executor().advance_clock(Duration::from_millis(299));
        window.run_until_parked();
        window.update(|_, cx| assert_eq!(cache.read(cx).target, 1024));
        window.executor().advance_clock(Duration::from_millis(1));
        window.run_until_parked();
        window.update(|_, cx| assert_eq!(cache.read(cx).target, 256));
        draw_sizing(window, second.sizing(2).unwrap(), 201.);
        window.run_until_parked();
        draw_sizing(window, second.sizing(2).unwrap(), 201.);
        window.update(|window, cx| {
            assert_eq!(cache.read(cx).tier, 256);
            second.retain_consumers(&[]);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(
                cache.read(cx).item.is_none(),
                "last visible consumer releases immediately"
            );
            assert!(cache.read(cx).pending.is_none());
            first.release_all_images(window, cx);
            second.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_native_static_and_single_frame_animation_ignore_larger_tiers(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frame(image::Frame::new(image::RgbaImage::new(64, 32)))
            .unwrap();
        for (bytes, animated) in [
            (static_cover(64, 74), false),
            (gif, true),
            (decode::tests::gif(), true),
        ] {
            let (_directory, loaded) = loaded_cover(&bytes);
            let images = ArtworkImageRegistry::default();
            let view = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let cache = window.update(|_, cx| install_cover(&view, loaded, cx));
            draw_sizing(window, view.sizing(1).unwrap(), 200.);
            window.run_until_parked();
            let original = window.update(|_, cx| {
                assert_eq!(cache.read(cx).source.get().unwrap().animated, animated);
                cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .unwrap()
            });
            for side in [257., 800., 2048., 100.] {
                draw_sizing(window, view.sizing(1).unwrap(), side);
                window.update(|_, cx| {
                    let image = cache.read(cx);
                    assert!(image.pending.is_none());
                    assert!(Arc::ptr_eq(
                        &original,
                        &image.item.as_ref().unwrap().get().unwrap().unwrap()
                    ));
                    if animated {
                        assert!(image.downgrade.is_none());
                    }
                });
            }
            window.update(|window, cx| {
                view.release_all_images(window, cx);
                images.release_all_retired(window, cx);
                assert!(cache.read(cx).artwork.is_none());
            });
        }
    }

    #[gpui::test]
    fn artwork_actual_loader_uses_published_session_encoded_limit(cx: &mut gpui::TestAppContext) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("large-encoded.wav");
        cover_wave(&path, &decode::tests::large_encoded_bmp());
        let track = rivu_protocol::test_support::artwork_track(path);
        let manager = rivu_metadata::artwork::ArtworkManager::new(directory.path());
        let loaded = manager.load(crate::artwork_source(&track)).unwrap();
        assert_eq!(
            std::fs::metadata(loaded.path()).unwrap().len(),
            9 * 1024 * 1024
        );
        let lower = rivu_metadata::artwork::ArtworkManager::new(directory.path())
            .with_max_image_bytes(8 * 1024 * 1024);
        assert!(lower.load(crate::artwork_source(&track)).is_none());
        let images = ArtworkImageRegistry::new(2);
        let view = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            view.replace(Some(loaded), cx);
            view.state
                .borrow()
                .current
                .as_ref()
                .unwrap()
                .owner
                .image
                .clone()
        });
        draw_sizing(window, view.sizing(1).unwrap(), 200.);
        window.run_until_parked();
        window.update(|window, cx| {
            let pixels = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert_eq!(u32::from(pixels.size(0).width), 200);
            assert_eq!(u32::from(pixels.size(0).height), 100);
            assert_eq!(&pixels.as_bytes(0).unwrap()[..4], &[40, 30, 210, 255]);
            view.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_encoded_policy_separates_failed_and_admitted_shared_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, admitted) = loaded_cover(&static_cover(800, 211));
        let manager = rivu_metadata::artwork::ArtworkManager::new(directory.path())
            .with_max_image_bytes(8 * 1024 * 1024);
        let track = rivu_protocol::test_support::artwork_track(directory.path().join("cover.wav"));
        let lower = manager.load(crate::artwork_source(&track)).unwrap();
        assert_eq!(lower.path(), admitted.path());
        let images = ArtworkImageRegistry::new(2);
        let rejected = ArtworkView::new(images.clone());
        let accepted = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let (rejected_cache, accepted_cache) = window.update(|_, cx| {
            let rejected = install_cover(&rejected, lower, cx);
            let accepted = install_cover(&accepted, admitted, cx);
            assert_ne!(rejected, accepted);
            (rejected, accepted)
        });
        // Real reader limits are covered above. Model a remembered terminal
        // result on the lower-policy lease without mutating the shared original.
        rejected_cache.update(window, |image, cx| {
            image.item = Some(ImageCacheItem::from_future(
                async { Err(ImageCacheError::Asset("controlled rejection".into())) },
                cx,
            ));
        });
        window.run_until_parked();
        window.update(|_, cx| {
            assert!(
                rejected_cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .is_err()
            );
        });
        draw_cover(window, &accepted);
        window.run_until_parked();
        window.update(|window, cx| {
            assert!(
                accepted_cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .is_ok()
            );
            assert!(
                rejected_cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .is_err()
            );
            rejected.release_all_images(window, cx);
            accepted.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_cover_actual_loader_shares_animation_and_retires_failed_and_live_scenes(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        for reject in [false, true] {
            let path = directory.path().join(format!("cover-{reject}.wav"));
            // The rejected cover is admitted by metadata (manager.load below
            // succeeds) and fails only in GUI decode.
            let gif = if reject {
                decode::tests::gif_with_corrupt_second_frame()
            } else {
                decode::tests::gif()
            };
            cover_wave(&path, &gif);
            let manager = rivu_metadata::artwork::ArtworkManager::new(directory.path());
            let loaded = manager
                .load(crate::artwork_source(
                    &rivu_protocol::test_support::artwork_track(path),
                ))
                .unwrap();
            let images = ArtworkImageRegistry::default();
            let first = ArtworkView::new(images.clone());
            let second = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let cache = window.update(|_, cx| {
                first.replace(Some(loaded.clone()), cx);
                second.replace(Some(loaded), cx);
                assert!(!first.is_animated(cx));
                assert!(!second.is_animated(cx));
                assert!(Rc::ptr_eq(
                    &first.state.borrow().current.as_ref().unwrap().owner,
                    &second.state.borrow().current.as_ref().unwrap().owner,
                ));
                first
                    .state
                    .borrow()
                    .current
                    .as_ref()
                    .unwrap()
                    .owner
                    .image
                    .clone()
            });
            let old_ambient = first.source().unwrap().ambient_image();
            draw_cover(window, &first);
            window.run_until_parked();
            draw_cover(window, &second);
            let pixels = window.update(|_, cx| {
                let result = cache.read(cx).item.as_ref().unwrap().get().unwrap();
                if reject {
                    assert!(result.is_err());
                    assert!(!first.is_animated(cx));
                    assert!(!second.is_animated(cx));
                    None
                } else {
                    let decoded = result.unwrap();
                    assert_eq!(decoded.frame_count(), 2);
                    assert_eq!(decoded.delay(1).numer_denom_ms(), (130, 1));
                    assert!(first.is_animated(cx));
                    assert!(second.is_animated(cx));
                    Some(Arc::downgrade(&decoded))
                }
            });
            let old_scene = first.sizing(1).unwrap(); // Scene captured after decode completed.
            window.update(|window, cx| {
                first.replace(None, cx);
                assert!(!first.is_animated(cx));
                images.release_retired(window);
                window.simulate_next_frame(cx);
            });
            if let Some(pixels) = &pixels {
                assert!(pixels.upgrade().is_some());
            }
            window.update(|window, cx| {
                second.replace(None, cx);
                images.release_retired(window);
                window.simulate_next_frame(cx);
            });
            if let Some(pixels) = &pixels {
                assert!(pixels.upgrade().is_none());
            }
            draw_sizing(window, old_scene, 64.);
            draw_source(window, old_ambient);
            window.run_until_parked();
            window.update(|_, cx| {
                let image = cache.read(cx);
                assert!(
                    image.artwork.is_none(),
                    "released scenes must not protect old files"
                );
                assert!(
                    image.item.is_none(),
                    "released scenes must not restart cover decoding"
                );
                assert!(
                    image.ambient.is_none(),
                    "released scenes must not restart filtering"
                );
            });
        }
    }

    #[gpui::test]
    fn artwork_budget_changes_retry_shared_covers_and_retire_previous_pixels(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&decode::tests::gif());
        let images = ArtworkImageRegistry::default();
        let first = ArtworkView::new(images.clone());
        let second = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            assert!(images.set_decode_budget(decode::tests::budget(1), cx));
            first.replace(Some(loaded.clone()), cx);
            second.replace(Some(loaded), cx);
            let first = first.state.borrow();
            let second = second.state.borrow();
            let first = &first.current.as_ref().unwrap().owner;
            let second = &second.current.as_ref().unwrap().owner;
            assert!(Rc::ptr_eq(first, second));
            first.image.clone()
        });

        draw_cover(window, &first);
        window.run_until_parked();
        window.update(|_, cx| {
            assert!(!images.set_decode_budget(decode::tests::budget(1), cx));
            assert!(
                cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .is_err()
            );

            assert!(images.set_decode_budget(decode::tests::budget(2), cx));
            assert!(cache.read(cx).item.is_none());
        });
        draw_cover(window, &first);
        window.run_until_parked();
        let pixels = window.update(|_, cx| {
            let decoded = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert_eq!(decoded.frame_count(), 2);
            assert!(!images.set_decode_budget(decode::tests::budget(2), cx));
            let unchanged = cache
                .read(cx)
                .item
                .as_ref()
                .unwrap()
                .get()
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&decoded, &unchanged));

            Arc::downgrade(&decoded)
        });

        window.update(|_, cx| {
            assert!(images.set_decode_budget(decode::tests::budget(1), cx));
            assert!(cache.read(cx).item.is_none());
            assert!(
                pixels.upgrade().is_some(),
                "old scene keeps its pixels until retirement"
            );
            assert_eq!(
                cache,
                second.state.borrow().current.as_ref().unwrap().owner.image,
            );
        });
        draw_cover(window, &second);
        window.run_until_parked();
        window.update(|_, cx| {
            assert_eq!(cache.read(cx).replaced.len(), 1);
        });
        // The first cover still names the previous budget generation until it
        // repaints. The second cover cannot acknowledge on its behalf.
        draw_cover(window, &first);
        window.update(|window, cx| {
            assert!(
                cache
                    .read(cx)
                    .item
                    .as_ref()
                    .unwrap()
                    .get()
                    .unwrap()
                    .is_err()
            );
            images.release_retired(window);
            assert!(pixels.upgrade().is_some());
            window.simulate_next_frame(cx);
        });
        assert!(pixels.upgrade().is_none());
        window.update(|window, cx| {
            first.release_all_images(window, cx);
            second.release_all_images(window, cx);
            images.release_all_retired(window, cx);
        });
    }

    type DecodeResult = Result<Arc<RenderImage>, ImageCacheError>;

    struct ControlledDecode {
        sender: Option<futures::channel::oneshot::Sender<DecodeResult>>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ControlledDecode {
        fn new() -> (Self, impl Future<Output = DecodeResult> + Send + 'static) {
            let (sender, receiver) = futures::channel::oneshot::channel();
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker = finished.clone();
            (
                Self {
                    sender: Some(sender),
                    finished,
                },
                async move {
                    let result = receiver.await.expect("controlled decode sender is held");
                    worker.store(true, std::sync::atomic::Ordering::Release);
                    result
                },
            )
        }

        fn before_retirement(
            &mut self,
            finished: bool,
            window: &mut gpui::VisualTestContext,
        ) -> std::sync::Weak<RenderImage> {
            let pixels = Arc::new(RenderImage::new([image::Frame::new(
                image::RgbaImage::new(1, 1),
            )]));
            let weak = Arc::downgrade(&pixels);
            if finished {
                self.sender.take().unwrap().send(Ok(pixels)).unwrap();
                while !self.finished.load(std::sync::atomic::Ordering::Acquire) {
                    assert!(window.background_executor.tick());
                }
            }
            weak
        }

        fn assert_retired(&self, pixels: std::sync::Weak<RenderImage>) {
            if let Some(pending) = &self.sender {
                assert!(pending.is_canceled());
            }
            assert!(pixels.upgrade().is_none());
        }
    }

    #[gpui::test]
    fn artwork_budget_change_cancels_pending_completion_before_it_can_publish(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&decode::tests::gif());
        for worker_finished in [false, true] {
            let images = ArtworkImageRegistry::default();
            let view = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let cache = window.update(|_, cx| {
                assert!(images.set_decode_budget(decode::tests::budget(2), cx));
                install_cover(&view, loaded.clone(), cx)
            });
            let old_scene = view.sizing(1).unwrap();
            let (mut worker, decode) = ControlledDecode::new();
            window.update(|_, cx| {
                cache.update(cx, |image, cx| {
                    image.item = Some(ImageCacheItem::from_future(decode, cx));
                });
                assert!(!images.set_decode_budget(decode::tests::budget(2), cx));
            });
            window.run_until_parked();
            assert!(
                !worker.sender.as_ref().unwrap().is_canceled(),
                "unchanged budget must keep pending work"
            );

            let stale_pixels = worker.before_retirement(worker_finished, window);
            window.update(|_, cx| {
                assert!(images.set_decode_budget(decode::tests::budget(1), cx));
                assert!(cache.read(cx).item.is_none());
            });
            window.run_until_parked();
            worker.assert_retired(stale_pixels);
            // Even a scene built under the old policy now uses the new budget.
            draw_sizing(window, old_scene, 64.);
            window.run_until_parked();
            window.update(|window, cx| {
                assert!(
                    cache
                        .read(cx)
                        .item
                        .as_ref()
                        .unwrap()
                        .get()
                        .unwrap()
                        .is_err()
                );

                view.release_all_images(window, cx);
                images.release_all_retired(window, cx);
            });
        }
    }

    #[gpui::test]
    fn unrequested_artwork_scenes_cannot_load_after_their_lease_is_retired(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&decode::tests::gif());
        let images = ArtworkImageRegistry::default();
        let view = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let (cache, old_cover, old_ambient) = window.update(|_, cx| {
            let cache = install_cover(&view, loaded, cx);
            (
                cache,
                view.sizing(1).unwrap(),
                view.source().unwrap().ambient_image(),
            )
        });
        window.update(|window, cx| {
            assert!(view.cached_static_source(window, cx).is_none());
            assert!(cache.read(cx).item.is_none());
            assert!(view.replace(None, cx));
            assert_eq!(images.0.borrow().retired.len(), 1);
            images.release_retired(window);
            window.simulate_next_frame(cx);
            assert!(cache.read(cx).artwork.is_none());
        });
        draw_sizing(window, old_cover, 64.);
        draw_source(window, old_ambient);
        window.run_until_parked();
        window.update(|_, cx| {
            assert!(cache.read(cx).item.is_none());
            assert!(cache.read(cx).ambient.is_none());
        });
    }

    #[gpui::test]
    fn artwork_flush_cancels_source_and_ambient_before_or_after_worker_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_directory, loaded) = loaded_cover(&decode::tests::gif());
        for ambient in [false, true] {
            for worker_finished in [false, true] {
                let images = ArtworkImageRegistry::default();
                let view = ArtworkView::new(images.clone());
                let window = cx.add_empty_window();
                let cache = window.update(|_, cx| install_cover(&view, loaded.clone(), cx));
                let (mut worker, decode) = ControlledDecode::new();
                cache.update(window, |image, cx| {
                    let pending = ImageCacheItem::from_future(decode, cx);
                    if ambient {
                        image.item = Some(ImageCacheItem::from_future(
                            async {
                                Ok(Arc::new(RenderImage::new([image::Frame::new(
                                    image::RgbaImage::new(1, 1),
                                )])))
                            },
                            cx,
                        ));
                        image.ambient = Some(pending);
                    } else {
                        image.item = Some(pending);
                    }
                });
                window.run_until_parked();
                let stale_pixels = worker.before_retirement(worker_finished, window);
                window.update(|window, cx| {
                    view.replace(None, cx);
                    images.release_retired(window);
                    images.release_all_retired(window, cx);
                    assert!(cache.read(cx).artwork.is_none());
                    view.replace(Some(loaded.clone()), cx);
                    assert_ne!(
                        cache,
                        view.state.borrow().current.as_ref().unwrap().owner.image
                    );
                });
                window.run_until_parked();
                worker.assert_retired(stale_pixels);
                draw_cover(window, &view);
                window.run_until_parked();
                window.update(|window, cx| {
                    // An old pending frame callback cannot clear the reopened source.
                    window.simulate_next_frame(cx);
                    assert!(view.aspect_ratio(cx).is_some());
                    assert!(cache.read(cx).item.is_none());
                    assert!(cache.read(cx).ambient.is_none());
                    view.release_all_images(window, cx);
                    images.release_all_retired(window, cx);
                });
            }
        }
    }

    struct DecorationScene(Option<Img>);

    impl Render for DecorationScene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.take().expect("single-use test scene").size_full()
        }
    }

    fn draw_cover(window: &mut gpui::VisualTestContext, cover: &ArtworkView) {
        draw_sizing(window, cover.sizing(1).unwrap(), 64.);
    }

    fn draw_sizing(
        window: &mut gpui::VisualTestContext,
        sizing: ArtworkImageSizing,
        physical_side: f32,
    ) {
        struct Cover(ArtworkImageSizing, f32);
        impl Render for Cover {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let side = gpui::px(self.1 / window.scale_factor());
                div()
                    .size_full()
                    .children(self.0.request_image(side, window, cx))
            }
        }
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(64.), gpui::px(64.)),
            |_, cx| cx.new(|_| Cover(sizing, physical_side)).into_any_element(),
        );
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
            DecorationLoad::Ready(Ok(decoded)) => Arc::downgrade(decoded),
            _ => panic!("decoration must have completed its local SVG decode"),
        })
    }

    #[gpui::test]
    fn decoration_consumers_share_decode_until_the_last_frame_retirement(
        cx: &mut gpui::TestAppContext,
    ) {
        let window = cx.add_empty_window();
        let images = ArtworkImageRegistry::default();
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
        let images = ArtworkImageRegistry::default();
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
            let images = ArtworkImageRegistry::default();
            let first = window.update(|_, cx| images.decoration(0, VINYL, cx));
            let old_cache = first.owner.image.clone();
            let old_scene = first.image();
            let (mut worker, decode) = ControlledDecode::new();
            old_cache.update(window, |image, cx| {
                image.start(decode, cx);
            });
            window.run_until_parked();
            let stale_pixels = worker.before_retirement(worker_finished, window);
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
            worker.assert_retired(stale_pixels);
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
    fn native_animation_redraws_cached_consumers_without_invalidating_static_siblings(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{
            artwork_panel::{ArtworkPanel, ArtworkRuntime},
            layout::{Axis, Node, Panel},
        };
        use rivu_format::config::{ArtworkMode, ArtworkOptions, ArtworkStyle, ConfigSettings};
        use rivu_protocol::model::PlaybackStatus;

        struct CountedPanel {
            panel: Entity<ArtworkPanel>,
            renders: Rc<std::cell::Cell<usize>>,
        }
        impl Render for CountedPanel {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                self.panel
                    .clone()
                    .cached(gpui::StyleRefinement::default().size_full())
            }
        }
        struct Root {
            panels: [Entity<CountedPanel>; 2],
            images: ArtworkImageRegistry,
        }
        impl Render for Root {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let images = self.images.clone();
                div()
                    .relative()
                    .flex()
                    .size_full()
                    .children(self.panels.iter().map(|panel| {
                        div().size(gpui::px(128.)).child(
                            panel
                                .clone()
                                .cached(gpui::StyleRefinement::default().size_full()),
                        )
                    }))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _| images.release_retired(window),
                        )
                        .absolute()
                        .size_full(),
                    )
            }
        }

        cx.update(|cx| {
            crate::gui::appearance::theme::set_theme(
                rivu_presentation::theme::Theme::default(),
                cx,
            );
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
        });
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let manager = Arc::new(rivu_metadata::artwork::ArtworkManager::new(
            directory.path(),
        ));
        let tracks: Vec<_> = [
            ("still.wav", static_cover(128, 79)),
            ("animated.wav", decode::tests::gif()),
        ]
        .into_iter()
        .map(|(name, bytes)| {
            let path = directory.path().join(name);
            cover_wave(&path, &bytes);
            let track = rivu_protocol::test_support::artwork_track(path);
            assert!(manager.load(crate::artwork_source(&track)).is_some());
            track
        })
        .collect();
        let images = ArtworkImageRegistry::default();
        let owner = cx.update(|cx| {
            let default = cx
                .new(|cx| DefaultArtwork::new(&rivu_presentation::i18n::Localizer::new("en"), cx));
            cx.new(|cx| {
                ArtworkRuntime::new(
                    manager,
                    default,
                    images.clone(),
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                )
            })
        });
        let mut panels = crate::gui::workspace::WorkspacePanels::default();
        let mut sync = |track, rotating, cx: &mut gpui::TestAppContext| {
            let config = ConfigSettings {
                animation_fps: 0,
                ..ConfigSettings::default()
            };
            panels.reconcile(&crate::gui::layout::Layout {
                root: Some(Node::Split {
                    id: 90,
                    axis: Axis::Horizontal,
                    boundaries: vec![0.5],
                    children: [1, 2]
                        .into_iter()
                        .map(|id| Node::Tabs {
                            id: id + 10,
                            active: id,
                            panels: vec![Panel {
                                id,
                                kind: "artwork".into(),
                                show_tab_bar: false,
                                audio_visualizer: None,
                                artwork: Some(ArtworkOptions {
                                    mode: if rotating && id == 1 {
                                        ArtworkMode::Rotating
                                    } else {
                                        ArtworkMode::Static
                                    },
                                    style: ArtworkStyle::Plain,
                                    ..(&config).into()
                                }),
                            }],
                        })
                        .collect(),
                }),
                next_id: 200,
                locked: true,
            });
            owner.update(cx, |owner, cx| {
                owner.sync(
                    Some(track),
                    if rotating {
                        PlaybackStatus::Playing
                    } else {
                        PlaybackStatus::Paused
                    },
                    &config,
                    &panels,
                    true,
                    cx,
                );
            });
        };
        sync(&tracks[0], true, cx);
        let renders: [_; 2] = std::array::from_fn(|_| Rc::new(std::cell::Cell::new(0)));
        let window = cx.open_window(gpui::size(gpui::px(320.), gpui::px(160.)), {
            let owner = owner.clone();
            let images = images.clone();
            let renders = renders.clone();
            move |window, cx| {
                window.activate_window();
                Root {
                    panels: std::array::from_fn(|index| {
                        let panel =
                            cx.new(|cx| ArtworkPanel::new(index as u64 + 1, owner.clone(), cx));
                        cx.new(|_| CountedPanel {
                            panel,
                            renders: Rc::clone(&renders[index]),
                        })
                    }),
                    images,
                }
            }
        });
        let redraw = Rc::new(std::cell::Cell::new(false));
        let consumers = window
            .read_with(cx, |root, cx| {
                root.panels
                    .each_ref()
                    .map(|panel| panel.read(cx).panel.clone())
            })
            .unwrap();
        cx.update(|cx| {
            for panel in &consumers {
                let redraw = Rc::clone(&redraw);
                cx.observe(panel, move |_, _| redraw.set(true)).detach();
            }
        });
        let draw = |cx: &mut gpui::TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        };
        let settle = |cx: &mut gpui::TestAppContext| {
            for _ in 0..16 {
                redraw.set(false);
                cx.run_until_parked();
                draw(cx);
                cx.run_until_parked();
                if !redraw.replace(false) {
                    return;
                }
            }
            panic!("artwork decode/prepaint notifications did not settle");
        };
        let tick = |cx: &mut gpui::TestAppContext| {
            redraw.set(false);
            let callbacks = cx
                .update_window(window.into(), |_, window, cx| {
                    window.simulate_next_frame(cx)
                })
                .unwrap();
            assert!(callbacks > 0, "a native animation frame must be pending");
            assert!(
                redraw.get(),
                "a native tick must invalidate a mounted consumer"
            );
            draw(cx);
        };
        settle(cx);
        let before = renders.each_ref().map(|count| count.get());
        for _ in 0..3 {
            tick(cx);
        }
        assert!(
            renders[0].get() > before[0],
            "rotating cached output must be rebuilt"
        );
        assert_eq!(renders[1].get(), before[1], "static sibling stays cached");
        cx.update(|cx| {
            assert!(owner.read(cx).rotation_angle(1) > 0.);
            assert_eq!(owner.read(cx).rotation_angle(2), 0.);
        });

        // Source animation must redraw both cached Static consumers without a root event.
        sync(&tracks[1], false, cx);
        settle(cx);
        cx.update(|cx| {
            assert_eq!(owner.read(cx).rotation_angle(1), 0.);
            assert_eq!(owner.read(cx).rotation_angle(2), 0.);
            assert!(
                images
                    .0
                    .borrow()
                    .entries
                    .values()
                    .filter_map(Weak::upgrade)
                    .any(|lease| {
                        lease
                            .image
                            .read(cx)
                            .item
                            .as_ref()
                            .and_then(ImageCacheItem::get)
                            .and_then(Result::ok)
                            .is_some_and(|image| image.frame_count() == 2)
                    })
            );
        });
        let before = renders.each_ref().map(|count| count.get());
        for _ in 0..3 {
            tick(cx);
        }
        assert!(
            renders
                .iter()
                .zip(before)
                .all(|(renders, before)| renders.get() > before)
        );
        cx.update_window(window.into(), |_, window, cx| {
            owner.update(cx, |owner, cx| owner.release_all_images(window, cx));
            images.release_all_retired(window, cx);
        })
        .unwrap();
    }

    #[gpui::test]
    fn artwork_source_animation_uses_one_owner_gate_across_panels_and_suspension(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{
            artwork_panel::{ArtworkPanel, ArtworkRuntime},
            layout::{Axis, Node, Panel},
        };
        use rivu_format::config::{ArtworkMode, ArtworkOptions, ArtworkStyle, ConfigSettings};
        use rivu_protocol::model::PlaybackStatus;

        cx.update(|cx| {
            crate::gui::appearance::theme::set_theme(
                rivu_presentation::theme::Theme::default(),
                cx,
            );
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
        });
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("paced.wav");
        cover_wave(&path, &decode::tests::gif());
        let track = rivu_protocol::test_support::artwork_track(path);
        let manager = Arc::new(rivu_metadata::artwork::ArtworkManager::new(
            directory.path(),
        ));
        assert!(manager.load(crate::artwork_source(&track)).is_some());
        let images = ArtworkImageRegistry::default();
        let owner = cx.update(|cx| {
            let default = cx
                .new(|cx| DefaultArtwork::new(&rivu_presentation::i18n::Localizer::new("en"), cx));
            cx.new(|cx| {
                ArtworkRuntime::new(
                    manager,
                    default,
                    images.clone(),
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                )
            })
        });
        let mut panels = crate::gui::workspace::WorkspacePanels::default();
        let mut sync = |mode, status, fps, visible, cx: &mut gpui::TestAppContext| {
            let config = ConfigSettings {
                animation_fps: fps,
                ..ConfigSettings::default()
            };
            let root = Node::Split {
                id: 90,
                axis: Axis::Horizontal,
                boundaries: vec![0.5],
                children: [1, 2]
                    .into_iter()
                    .map(|id| Node::Tabs {
                        id: id + 10,
                        active: id,
                        panels: vec![Panel {
                            id,
                            kind: "artwork".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            artwork: Some(ArtworkOptions {
                                mode,
                                style: ArtworkStyle::Plain,
                                ..(&config).into()
                            }),
                        }],
                    })
                    .collect(),
            };
            panels.reconcile(&crate::gui::layout::Layout {
                root: Some(root),
                next_id: 200,
                locked: true,
            });
            owner.update(cx, |owner, cx| {
                owner.sync(Some(&track), status, &config, &panels, visible, cx);
            });
        };
        sync(ArtworkMode::Static, PlaybackStatus::Paused, 0, true, cx);
        let window = cx.add_empty_window();
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        let panels = [1, 2].map(|id| window.new(|cx| ArtworkPanel::new(id, owner.clone(), cx)));
        let draw = |window: &mut gpui::VisualTestContext| {
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(128.), gpui::px(128.)),
                |_, _| div().children(panels.iter().cloned()).into_any_element(),
            );
        };

        // Loading a static/paused panel must not invent a clock. The existing
        // decode-completion redraw then starts source animation without playback.
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0
        );
        window.run_until_parked();
        draw(window);
        window.run_until_parked();
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );

        for mode in [ArtworkMode::Static, ArtworkMode::Rotating] {
            for status in [PlaybackStatus::Paused, PlaybackStatus::Playing] {
                for fps in [1, 0] {
                    sync(mode, status, fps, true, &mut window.cx);
                    draw(window);
                    assert_eq!(
                        window.update(|window, cx| window.simulate_next_frame(cx)),
                        1,
                        "two source cursors and rotation share one native request",
                    );
                    draw(window);
                    assert_eq!(
                        window.update(|window, cx| window.simulate_next_frame(cx)),
                        usize::from(fps == 0),
                        "positive FPS waits at the owner deadline; zero uses native frames",
                    );
                }
            }
        }

        let angles = |window: &mut gpui::VisualTestContext| {
            window.update(|_, cx| [1, 2].map(|id| owner.read(cx).rotation_angle(id)))
        };
        let before_reduced_motion = angles(window);
        window.update(|_, cx| cx.set_reduce_motion(true));
        for _ in 0..3 {
            draw(window);
            // A native callback already registered before suspension may retire.
            // Its count is not evidence that either consumer advanced.
            window.update(|window, cx| window.simulate_next_frame(cx));
            assert_eq!(angles(window), before_reduced_motion);
        }
        window.update(|_, cx| cx.set_reduce_motion(false));
        for _ in 0..2 {
            draw(window);
            window.update(|window, cx| window.simulate_next_frame(cx));
        }
        assert!(
            angles(window)
                .into_iter()
                .zip(before_reduced_motion)
                .all(|(after, before)| after > before),
            "both visible rotations resume after reduced motion"
        );

        window.deactivate_window();
        let before_unfocused_rotation = angles(window);
        draw(window);
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(
            angles(window)
                .into_iter()
                .zip(before_unfocused_rotation)
                .all(|(after, before)| after > before),
            "focus loss alone does not stop an actively playing rotation"
        );
        sync(
            ArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        let unfocused_static = angles(window);
        for _ in 0..3 {
            window.update(|window, cx| window.simulate_next_frame(cx));
            draw(window);
            assert_eq!(angles(window), unfocused_static);
        }
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1,
            "a focused animated source resumes even while rotation is static"
        );
        assert_eq!(angles(window), unfocused_static);

        // Hide with a native callback outstanding. It may drain once, but cannot
        // restart the old generation while the panels are hidden.
        sync(
            ArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        sync(
            ArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            false,
            &mut window.cx,
        );
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0
        );
        sync(
            ArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );
        window.run_until_parked();
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );

        sync(
            ArtworkMode::Rotating,
            PlaybackStatus::Playing,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        window.update(|window, cx| window.simulate_next_frame(cx));
        sync(
            ArtworkMode::Rotating,
            PlaybackStatus::Playing,
            0,
            false,
            &mut window.cx,
        );
        let hidden_angles = angles(window);
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(angles(window), hidden_angles);
        sync(
            ArtworkMode::Rotating,
            PlaybackStatus::Playing,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(
            angles(window),
            hidden_angles,
            "the first resumed frame must not catch up hidden rotation time"
        );
        window.update(|window, cx| {
            owner.update(cx, |owner, cx| owner.release_all_images(window, cx));
            images.release_all_retired(window, cx);
        });
    }

    #[gpui::test]
    fn artwork_panel_visibility_style_and_layout_transfer_real_decorations(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            crate::gui::appearance::theme::set_theme(
                rivu_presentation::theme::Theme::default(),
                cx,
            );
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
        });
        use super::super::{
            artwork_panel::{ArtworkPanel, ArtworkRuntime},
            layout::{Axis, Node, Panel},
        };
        use rivu_format::config::{ArtworkOptions, ArtworkStyle, ConfigSettings};
        use rivu_protocol::model::PlaybackStatus;

        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(rivu_metadata::artwork::ArtworkManager::new(
            directory.path(),
        ));
        let images = ArtworkImageRegistry::default();
        let config = ConfigSettings::default();
        let artwork = cx.update(|cx| {
            let default = cx
                .new(|cx| DefaultArtwork::new(&rivu_presentation::i18n::Localizer::new("en"), cx));
            cx.new(|cx| {
                ArtworkRuntime::new(
                    manager,
                    default,
                    images.clone(),
                    cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                )
            })
        });
        let root = |panels: &[(u64, ArtworkStyle, bool)]| Node::Split {
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
                            kind: "artwork".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            artwork: Some(ArtworkOptions {
                                style,
                                ..(&config).into()
                            }),
                        },
                        Panel {
                            id: id + 100,
                            kind: "queue".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            artwork: None,
                        },
                    ],
                })
                .collect(),
        };
        let mut panels = crate::gui::workspace::WorkspacePanels::default();
        let mut sync = |root: Option<&Node>, visible, cx: &mut gpui::TestAppContext| {
            panels.reconcile(&crate::gui::layout::Layout {
                root: root.cloned(),
                next_id: 200,
                locked: true,
            });
            artwork.update(cx, |artwork, cx| {
                artwork.sync(None, PlaybackStatus::Stopped, &config, &panels, visible, cx);
            });
        };
        let initial = root(&[
            (1, ArtworkStyle::Vinyl, true),
            (2, ArtworkStyle::Vinyl, true),
        ]);
        sync(Some(&initial), true, cx);
        let window = cx.add_empty_window();
        let panel = window.new(|cx| ArtworkPanel::new(2, artwork.clone(), cx));
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
            (1, ArtworkStyle::Vinyl, false),
            (2, ArtworkStyle::Vinyl, true),
        ]);
        sync(Some(&one_hidden), true, &mut window.cx);
        window.update(|window, _| images.release_retired(window));
        window.update(|window, cx| window.simulate_next_frame(cx));
        assert!(vinyl_pixels.upgrade().is_some());

        let switched = root(&[
            (1, ArtworkStyle::Vinyl, false),
            (2, ArtworkStyle::Glass, true),
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
        let side = DefaultArtworkKey::Cover(side);
        if !sizes.touch(side) {
            while sizes.evict_for(side).is_some() {}
            sizes.insert(side);
        }
    }

    #[gpui::test]
    fn material_backgrounds_reuse_default_cache_only_for_missing_or_failed_covers(
        cx: &mut gpui::TestAppContext,
    ) {
        for ambient in [false, true] {
            for cover in ["missing", "real", "corrupt"] {
                let bytes = if cover == "corrupt" {
                    gif_with_corrupt_second_frame()
                } else {
                    static_cover(64, 200)
                };
                let (_directory, loaded) = loaded_cover(&bytes);
                let view = ArtworkView::new(ArtworkImageRegistry::default());
                let window = cx.add_empty_window();
                let fallback = window.update(|_, cx| {
                    if cover != "missing" {
                        view.replace(Some(loaded), cx);
                    }
                    DefaultArtwork::new(&rivu_presentation::i18n::Localizer::new("en"), cx)
                });
                for _ in 0..3 {
                    draw_source(window, fallback.background_image(view.source(), ambient));
                    window.run_until_parked();
                }
                if ambient && cover != "real" {
                    let theme = rivu_presentation::theme::palette(
                        rivu_presentation::theme::Theme::RivuDark,
                    );
                    let cover = window.update(|window, _| {
                        let side = gpui::px(ambient::SIDE as f32) / window.scale_factor();
                        fallback.image(theme, side, window)
                    });
                    draw_source(window, cover);
                    window.run_until_parked();
                }
                window.update(|_, cx| {
                    let cache = fallback.cache.read(cx);
                    if cover == "real" {
                        // Neither a pending decode nor real artwork gets a logo layer.
                        assert!(cache.sizes.recent.is_empty());
                    } else {
                        assert_eq!(cache.sizes.recent.len(), if ambient { 2 } else { 1 });
                        let side = if ambient {
                            DefaultArtworkKey::Atmosphere
                        } else {
                            cache.sizes.recent[0]
                        };
                        let source = cache.source(side);
                        let cover_source = cache.source(DefaultArtworkKey::Cover(ambient::SIDE));
                        let image = cx
                            .fetch_asset::<DefaultArtworkBitmap>(&source)
                            .unwrap()
                            .unwrap();
                        if ambient {
                            let parsed = source
                                .rasterizer
                                .renderer
                                .parse_svg(rivu_presentation::DEFAULT_ARTWORK_SVG)
                                .unwrap();
                            let side = gpui::DevicePixels(ambient::SIDE as i32);
                            let raw = source
                                .rasterizer
                                .renderer
                                .render_parsed(&parsed, SvgSize::ExactSize(gpui::size(side, side)))
                                .unwrap();
                            let expected = ambient::filtered(&raw);
                            assert_eq!(image.size(0), expected.size(0));
                            assert_eq!(image.as_bytes(0), expected.as_bytes(0));
                            assert_ne!(image.as_bytes(0), raw.as_bytes(0));
                            let cover = cx
                                .fetch_asset::<DefaultArtworkBitmap>(&cover_source)
                                .unwrap()
                                .unwrap();
                            assert_eq!(cover.as_bytes(0), raw.as_bytes(0));
                        }
                    }
                    if cover == "corrupt" {
                        let image = view.source().unwrap().sizing.image;
                        assert!(matches!(
                            image.read(cx).item.as_ref().unwrap().get(),
                            Some(Err(_)),
                        ));
                    }
                });
            }
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
            assert!(sizes.recent.contains(&DefaultArtworkKey::Cover(180)));
            assert!(sizes.recent.contains(&DefaultArtworkKey::Cover(512)));
            assert_eq!(
                sizes.bytes,
                sizes
                    .recent
                    .iter()
                    .map(|key| u64::from(key.side()).pow(2) * 4)
                    .sum::<u64>()
            );
        }
        let bytes = sizes.bytes;
        let count = sizes.recent.len();
        for _ in 0..100 {
            assert!(sizes.touch(DefaultArtworkKey::Cover(180)));
            assert!(sizes.touch(DefaultArtworkKey::Cover(512)));
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
        assert!(!sizes.recent.contains(&DefaultArtworkKey::Cover(2048)));
        assert!(sizes.recent.contains(&DefaultArtworkKey::Cover(2047)));
        assert!(sizes.recent.contains(&DefaultArtworkKey::Cover(180)));
        assert!(sizes.bytes <= DEFAULT_ARTWORK_MAX_BYTES);
    }

    use rivu_protocol::model::Track;

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
            replaygain_track_gain: None,
            favorite: false,
            decoder_hint: Default::default(),
            missing: false,
            play_count: 0,
            last_played: None,
        }
    }

    #[test]
    fn physical_slot_retires_only_matching_completion_before_latest_demand() {
        let mut loader = ArtworkLoader::default();
        let first = track(1, "original");
        let skipped = track(2, "original");
        let mut latest = first.clone();
        latest.path = "/moved/latest.wav".into();
        let active = ArtworkIdentity::new(crate::artwork_source(&first));
        let replacement = ArtworkIdentity::new(crate::artwork_source(&latest));
        assert!(loader.prepare(crate::artwork_source(&first)).is_some());
        assert!(loader.prepare(crate::artwork_source(&skipped)).is_none());
        assert!(loader.prepare(crate::artwork_source(&latest)).is_none());
        assert!(!loader.finish(&replacement));
        assert!(loader.prepare(crate::artwork_source(&latest)).is_none());
        assert!(loader.finish(&active));
        assert_eq!(
            loader.prepare(crate::artwork_source(&latest)).unwrap().path,
            latest.path
        );
        assert!(!loader.finish(&active));
        assert!(loader.prepare(crate::artwork_source(&first)).is_none());
        assert!(loader.finish(&replacement));
        assert!(loader.active.is_none());
    }

    #[test]
    fn negative_demand_is_retained_only_for_the_visible_exact_source() {
        let original = track(1, "original");
        let identity = ArtworkIdentity::new(crate::artwork_source(&original));
        let mut relocated = original.clone();
        relocated.path = "/relocated/track.wav".into();
        let changed = track(1, "replacement");
        let other = track(2, "original");
        for next in [relocated, changed, other] {
            let mut loader = ArtworkLoader::default();
            loader.remember_result(identity.clone(), false);
            loader.retain_negative(Some(crate::artwork_source(&original)));
            assert!(loader.prepare(crate::artwork_source(&original)).is_none());
            loader.retain_negative(Some(crate::artwork_source(&next)));
            assert!(loader.prepare(crate::artwork_source(&next)).is_some());
            // Hiding clears demand, never the still-running physical slot.
            loader.retain_negative(None);
            assert!(loader.prepare(crate::artwork_source(&original)).is_none());
            assert!(loader.finish(&ArtworkIdentity::new(crate::artwork_source(&next))));
            assert!(loader.prepare(crate::artwork_source(&original)).is_some());
        }
        let mut loader = ArtworkLoader::default();
        loader.remember_result(identity.clone(), false);
        loader.retain_negative(None);
        assert!(loader.prepare(crate::artwork_source(&original)).is_some());
        assert!(loader.finish(&identity));
        loader.remember_result(identity.clone(), false);
        loader.remember_result(identity, true);
        assert!(loader.prepare(crate::artwork_source(&original)).is_some());
    }
}
