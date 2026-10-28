use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    path::Path,
    rc::{Rc, Weak},
    sync::{Arc, OnceLock},
    time::Duration,
};

use crate::artwork::{Artwork as LoadedArtwork, ArtworkIdentity};
use rivu_protocol::model::Track;

use gpui::{
    App, Asset, Context, Entity, EntityId, ImageCache, ImageCacheError, ImageCacheItem, Img,
    ParsedSvg, Pixels, Render, RenderImage, Resource, SvgRenderer, SvgSize, Task, Window, canvas,
    div, img, prelude::*, rgb,
};

mod decode;

#[derive(Default)]
struct ArtworkImagesState {
    budget: decode::Budget,
    entries: HashMap<(Arc<Path>, usize), Weak<ArtworkImageOwner>>,
    decorations: [Weak<DecorationImageOwner>; 6],
    retired: Vec<RetiredArtworkImage>,
    pending_retired: Option<Vec<RetiredArtworkImage>>,
    retirement_generation: u64,
    next_consumer: u64,
}

/// Shared by one window, never by the application session. Weak entries are
/// removed with their last consumer instead of retaining an idle decode cache.
#[derive(Clone, Default)]
pub(super) struct ArtworkImages(Rc<RefCell<ArtworkImagesState>>);

impl ArtworkImages {
    pub(super) fn new(retained_mb: u32) -> Self {
        Self(Rc::new(RefCell::new(ArtworkImagesState {
            budget: decode::Budget::new(retained_mb),
            ..ArtworkImagesState::default()
        })))
    }

    pub(super) fn set_decode_budget_mb(&self, retained_mb: u32, cx: &mut App) -> bool {
        self.set_decode_budget(decode::Budget::new(retained_mb), cx)
    }

    fn set_decode_budget(&self, budget: decode::Budget, cx: &mut App) -> bool {
        let mut state = self.0.borrow_mut();
        if state.budget == budget {
            return false;
        }
        state.budget = budget;
        let ArtworkImagesState {
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
                let pixels = image.take_pixels();
                if image.views.is_empty() {
                    pixels.collect::<Vec<_>>()
                } else {
                    image.generation = image.generation.wrapping_add(1);
                    let generation = image.generation;
                    image
                        .replaced
                        .extend(pixels.map(|image| ReplacedArtworkImage { image, generation }));
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

    fn acquire(&self, artwork: LoadedArtwork, cx: &mut App) -> Rc<ArtworkImageOwner> {
        let key = (artwork.path(), artwork.max_image_bytes());
        let budget = {
            let state = self.0.borrow();
            if let Some(owner) = state.entries.get(&key).and_then(Weak::upgrade) {
                return owner;
            }
            state.budget
        };
        let owner = Rc::new(ArtworkImageOwner {
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

    fn retire(&self, image: ArtworkViewImage, cx: &App) {
        // Measurement handles can already be held by a scene before Img starts
        // its first decode; those owners must also become terminal at the edge.
        if !image.consumers.is_empty() || image.owner.image.read(cx).item.is_some() {
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
    Decoration(Rc<DecorationImageOwner>),
    Decoded(Arc<RenderImage>),
}

impl RetiredArtworkImage {
    fn release(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Artwork(image) => image.release(window, cx),
            Self::Demand(image, consumers) => image.update(cx, |image, cx| {
                for consumer in consumers {
                    image.demands.remove(&consumer);
                    image.views.remove(&consumer);
                }
                image.reconcile_demand(window, cx);
            }),
            Self::Decoration(owner) => owner.release(window, cx),
            Self::Decoded(decoded) => cx.drop_image(decoded, Some(window)),
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
    key: (Arc<Path>, usize),
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
            registry.borrow_mut().entries.remove(&self.key);
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
    consumers: HashMap<u64, u64>,
}

impl ArtworkViewImage {
    fn release(self, window: &mut Window, cx: &mut App) {
        if !self.consumers.is_empty() {
            self.owner.image.update(cx, |image, cx| {
                for consumer in self.consumers.into_values() {
                    image.demands.remove(&consumer);
                    image.views.remove(&consumer);
                }
                image.reconcile_demand(window, cx);
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
pub(super) struct ArtworkImageSizing {
    image: Entity<ArtworkImage>,
    token: u64,
}

impl ArtworkImageSizing {
    pub(super) fn request(&self, side: Pixels, window: &mut Window, cx: &mut App) {
        let view = window.current_view();
        self.image.update(cx, |image, cx| {
            image.views.insert(self.token, (view, image.generation));
            image.flush_replaced();
            image.request(self.token, side, window, cx);
        });
    }
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
            consumers: HashMap::new(),
        });
        if let Some(old) = std::mem::replace(&mut state.current, next) {
            self.images.retire(old, cx);
        }
        true
    }

    pub(super) fn image(&self, cx: &App) -> Option<Img> {
        let state = self.state.borrow();
        let image = state.current.as_ref()?;
        // All consumers already render their default cover for None. A cached
        // decode error is terminal for this lease, not a new load each render.
        if image
            .owner
            .image
            .read(cx)
            .item
            .as_ref()
            .and_then(ImageCacheItem::get)
            .is_some_and(|result| result.is_err())
        {
            return None;
        }
        let mut element = img(image.artwork.path()).image_cache(&image.owner.image);
        // Img needs a stable ID to retain its animation cursor across frames.
        element.interactivity().element_id =
            Some(("artwork-cover", image.owner.image.entity_id()).into());
        Some(element)
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
        })
    }

    /// Measure the actual laid-out image surface, not an estimated panel width.
    pub(super) fn measured_image(&self, consumer: u64, cx: &App) -> Option<gpui::AnyElement> {
        let image = self.image(cx)?;
        let sizing = self.sizing(consumer)?;
        Some(
            canvas(
                move |bounds, window, cx| {
                    let side = bounds.size.width.max(bounds.size.height);
                    sizing.request(side, window, cx);
                    let mut image = image
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element();
                    let _ = image.prepaint_as_root(
                        bounds.origin,
                        bounds.size.map(Into::into),
                        window,
                        cx,
                    );
                    image
                },
                |_, mut image, window, cx| image.paint(window, cx),
            )
            .size_full()
            .into_any_element(),
        )
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

struct ArtworkSourceInfo {
    animated: bool,
    side: u32,
}

struct ReplacedArtworkImage {
    image: Arc<RenderImage>,
    generation: u64,
}

/// One immutable content path, shared maximum demand, and one in-flight decode.
struct ArtworkImage {
    artwork: LoadedArtwork,
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
    registry: Weak<RefCell<ArtworkImagesState>>,
    released: bool,
}

impl ArtworkImage {
    fn new(
        artwork: LoadedArtwork,
        budget: decode::Budget,
        registry: Weak<RefCell<ArtworkImagesState>>,
        cx: &mut App,
    ) -> Entity<Self> {
        let image = cx.new(|_| Self {
            artwork,
            budget,
            item: None,
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
            released: false,
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
        self.downgrade = None;
        self.failed_tier = None;
        [self.item.take(), self.pending.take().map(|(_, item)| item)]
            .into_iter()
            .filter_map(|item| item.and_then(|item| item.get()).and_then(Result::ok))
            .chain(
                std::mem::take(&mut self.replaced)
                    .into_iter()
                    .map(|retired| retired.image),
            )
    }

    fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.released = true;
        for decoded in self.take_pixels() {
            cx.drop_image(decoded, Some(window));
        }
    }

    fn request(
        &mut self,
        consumer: u64,
        side: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.released {
            return;
        }
        let physical = (f32::from(side).max(0.) * window.scale_factor()).ceil() as u32;
        let tier = decode::requested_tier(physical);
        if self.demands.insert(consumer, tier) != Some(tier) {
            self.reconcile_demand(window, cx);
        }
    }

    fn reconcile_demand(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.released {
            return;
        }
        let Some(target) = self.demands.values().copied().max() else {
            // The scene that used these pixels has already crossed a frame edge.
            self.target = decode::requested_tier(0);
            for decoded in self.take_pixels() {
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
        let artwork = self.artwork.clone();
        let budget = self.budget;
        let source = Arc::clone(&self.source);
        ImageCacheItem::from_future(
            async move {
                // Hold the source lease throughout background I/O/decode.
                let result = decode::load(&artwork.path(), artwork.max_image_bytes(), budget, tier);
                if let Err(error) = &result {
                    tracing::error!(?error, "artwork_decode_failed");
                }
                result.map(|decoded| {
                    source.get_or_init(|| ArtworkSourceInfo {
                        animated: decoded.animated,
                        side: decoded.source_side,
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
        let mut index = 0;
        while index < self.replaced.len() {
            if self.replaced[index].generation <= acknowledged {
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
}

impl ImageCache for ArtworkImage {
    fn load(
        &mut self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        debug_assert_eq!(resource, &Resource::from(self.artwork.path()));
        if self.released {
            return None;
        }
        if self.item.is_none() {
            self.tier = self.target;
            self.item = Some(self.decode_item(self.tier, cx));
        }
        let current = self.item.as_ref()?.use_image(window)?;
        // Animated sources always use their original frames, independent of tier.
        if self.source.get().is_some_and(|source| source.animated) {
            return Some(current);
        }
        if current.is_err() {
            return Some(current);
        }
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
            if result.is_ok() && useful {
                self.generation = self.generation.wrapping_add(1);
                if let Some(Ok(decoded)) = self.item.replace(pending).and_then(|item| item.get()) {
                    self.replaced.push(ReplacedArtworkImage {
                        image: decoded,
                        generation: self.generation,
                    });
                }
                // A cached sibling may never have requested this pending load.
                // Invalidate every measured consumer, and retain its old atlas
                // pixels until all consumers have prepainted the new generation.
                self.invalidate_consumers(cx);
                self.flush_replaced();
                self.tier = tier;
                self.failed_tier = None;
            } else if let Ok(decoded) = result {
                // A superseded downgrade must not replace a still-suitable
                // current image; a superseded upgrade must not grow it needlessly.
                self.retire_pixels(decoded);
            } else {
                self.failed_tier = Some(tier);
            }
        }
        let current = self.item.as_ref()?.use_image(window)?;
        let native_satisfies = current.as_ref().is_ok_and(|image| {
            let size = image.size(0);
            let side = size.width.0.max(size.height.0) as u32;
            self.source
                .get()
                .is_some_and(|source| source.side == side && self.target >= side)
        });
        if self.pending.is_none()
            && !native_satisfies
            && self.tier != self.target
            && self.failed_tier != Some(self.target)
        {
            self.pending = Some((self.target, self.decode_item(self.target, cx)));
            self.pending.as_ref().unwrap().1.use_image(window);
        }
        // Never replace a usable old image with a loading or failed upgrade.
        Some(current)
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
                pending.id == track.id
                    && pending.fingerprint == track.fingerprint
                    && pending.path == track.path
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
    // one rasterization at a time, not one worker per card or size.
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
                        .parse_svg(include_bytes!("../../data/artwork/default.svg"))?,
                );
            }
            let side = source.side as i32;
            source
                .rasterizer
                .renderer
                .render_parsed(
                    parsed.as_ref().expect("parsed default artwork"),
                    SvgSize::ExactSize(gpui::size(
                        gpui::DevicePixels(side),
                        gpui::DevicePixels(side),
                    )),
                )
                .map_err(Into::into)
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
        theme: &'static rivu_ui::theme::Palette,
        side: Pixels,
        window: &Window,
    ) -> Img {
        let side = default_artwork_side(side, window.scale_factor());
        let cache = self.cache.clone();
        let fallback = self.fallback.clone();
        img(move |window: &mut Window, cx: &mut App| {
            cache.update(cx, |cache, cx| cache.load(side, window, cx))
        })
        .with_fallback(move || rivu_ui::caption(theme, fallback.clone()).into_any_element())
    }
}

impl Render for Artwork {
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

    const VINYL: &[u8] = include_bytes!("../../data/artwork/vinyl.svg");

    fn cover_wave(path: &Path, image: &[u8]) {
        let mut picture = b"\0image/gif\0\x03\0".to_vec();
        picture.extend_from_slice(image);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&(picture.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&picture);
        let mut bytes = b"ID3\x03\0\0".to_vec();
        for shift in [21, 14, 7, 0] {
            bytes.push(((frame.len() >> shift) & 0x7f) as u8);
        }
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x02\0\0\0\0\x40");
        std::fs::write(path, bytes).unwrap();
    }

    fn loaded_cover(bytes: &[u8]) -> (tempfile::TempDir, LoadedArtwork) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        cover_wave(&path, bytes);
        let manager = crate::artwork::ArtworkManager::new(directory.path());
        let loaded = manager.load(&crate::artwork::tests::track(path)).unwrap();
        (directory, loaded)
    }

    // Each test selects distinct pixels so global same-source decode sharing
    // cannot make another concurrently running test own its release probe.
    fn static_cover(side: u32, red: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(side, side / 2, image::Rgba([red, 90, 110, 128]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
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
                    window,
                    cx,
                );
            });
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
        }
        impl Render for Panel {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                self.cover.measured_image(self.consumer, cx).unwrap()
            }
        }
        struct Root {
            first: Entity<Panel>,
            second: Entity<Panel>,
            side: Pixels,
            images: ArtworkImages,
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
        let images = ArtworkImages::default();
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
                }),
                second: cx.new(|_| Panel {
                    cover: view,
                    consumer: 2,
                    renders: second_renders,
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
            assert_eq!(cache.read(cx).tier, 256);
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
            assert_eq!(image.tier, 512);
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
            assert!(images.set_decode_budget_mb(128, cx));
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
        let images = ArtworkImages::default();
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
        request_cover_size(window, &view, 1, 800.);
        draw_cover(window, &view);
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
        // A 256 downgrade finishes after the user grows back to 512. The
        // current 1024 is still suitable; publishing 256 would visibly regress.
        window.update(|_, cx| {
            cache.update(cx, |image, cx| {
                image.target = 512;
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
        draw_cover(window, &view);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.tier, 1024);
            assert_eq!(image.pending.as_ref().unwrap().0, 512);
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
                    512,
                    ImageCacheItem::from_future(
                        async { Err(ImageCacheError::Asset("controlled decode failure".into())) },
                        cx,
                    ),
                ));
            });
        });
        window.run_until_parked();
        for _ in 0..3 {
            draw_cover(window, &view);
            window.update(|_, cx| {
                let image = cache.read(cx);
                assert!(image.pending.is_none());
                assert_eq!(image.failed_tier, Some(512));
                assert!(Arc::ptr_eq(
                    &original,
                    &image.item.as_ref().unwrap().get().unwrap().unwrap()
                ));
                assert!(view.image(cx).is_some());
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
        let images = ArtworkImages::default();
        let first = ArtworkView::new(images.clone());
        let second = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let cache = window.update(|_, cx| {
            first.replace(Some(loaded.clone()), cx);
            second.replace(Some(loaded), cx);
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
        request_cover_size(window, &first, 1, 256.);
        request_cover_size(window, &second, 2, 200.);
        draw_cover(window, &first);
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
        request_cover_size(window, &first, 1, 257.);
        draw_cover(window, &first);
        window.update(|_, cx| {
            let image = cache.read(cx);
            assert_eq!(image.target, 512);
            assert_eq!(image.pending.as_ref().unwrap().0, 512);
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
                512,
                "one in-flight decode"
            );
        });
        window.run_until_parked();
        draw_cover(window, &first);
        window.run_until_parked();
        draw_cover(window, &first);
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
        request_cover_size(window, &first, 1, 801.);
        draw_cover(window, &first);
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
        draw_cover(window, &second);
        window.run_until_parked();
        draw_cover(window, &second);
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
            let images = ArtworkImages::default();
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
            request_cover_size(window, &view, 1, 200.);
            draw_cover(window, &view);
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
                request_cover_size(window, &view, 1, side);
                draw_cover(window, &view);
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
                assert!(cache.read(cx).released);
            });
        }
    }

    #[gpui::test]
    fn artwork_actual_loader_uses_published_session_encoded_limit(cx: &mut gpui::TestAppContext) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("large-encoded.wav");
        cover_wave(&path, &decode::tests::large_encoded_bmp());
        let track = crate::artwork::tests::track(path);
        let manager = crate::artwork::ArtworkManager::new(directory.path());
        let loaded = manager.load(&track).unwrap();
        assert_eq!(loaded.max_image_bytes(), 16 * 1024 * 1024);
        assert_eq!(
            std::fs::metadata(loaded.path()).unwrap().len(),
            9 * 1024 * 1024
        );
        let lower = crate::artwork::ArtworkManager::new(directory.path())
            .with_max_image_bytes(8 * 1024 * 1024);
        assert!(lower.load(&track).is_none());
        let images = ArtworkImages::new(2);
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
        request_cover_size(window, &view, 1, 200.);
        draw_cover(window, &view);
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
            assert_eq!(u32::from(pixels.size(0).width), 256);
            assert_eq!(u32::from(pixels.size(0).height), 128);
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
        let manager = crate::artwork::ArtworkManager::new(directory.path())
            .with_max_image_bytes(8 * 1024 * 1024);
        let track = crate::artwork::tests::track(directory.path().join("cover.wav"));
        let lower = manager.load(&track).unwrap();
        assert_eq!(lower.path(), admitted.path());
        // Simulate an external cache-file change: the background reader must
        // still enforce each immutable session policy, including remembered errors.
        std::fs::write(admitted.path(), decode::tests::large_encoded_bmp()).unwrap();
        let images = ArtworkImages::new(2);
        let rejected = ArtworkView::new(images.clone());
        let accepted = ArtworkView::new(images.clone());
        let window = cx.add_empty_window();
        let (rejected_cache, accepted_cache) = window.update(|_, cx| {
            rejected.replace(Some(lower), cx);
            accepted.replace(Some(admitted), cx);
            let rejected = rejected
                .state
                .borrow()
                .current
                .as_ref()
                .unwrap()
                .owner
                .clone();
            let accepted = accepted
                .state
                .borrow()
                .current
                .as_ref()
                .unwrap()
                .owner
                .clone();
            assert!(!Rc::ptr_eq(&rejected, &accepted));
            (rejected.image.clone(), accepted.image.clone())
        });
        draw_cover(window, &rejected);
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
    fn artwork_cover_actual_loader_shares_animation_rejects_to_default_and_retires(
        cx: &mut gpui::TestAppContext,
    ) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        for reject in [false, true] {
            let path = directory.path().join(format!("cover-{reject}.wav"));
            let mut gif = decode::tests::gif();
            if reject {
                gif[6..10].copy_from_slice(&[0, 32, 0, 32]); // 8192-square header only
            }
            cover_wave(&path, &gif);
            let manager = crate::artwork::ArtworkManager::new(directory.path());
            let loaded = manager.load(&crate::artwork::tests::track(path)).unwrap();
            let images = ArtworkImages::default();
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
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(64.), gpui::px(64.)),
                |_, cx| {
                    let image = first.image(cx).unwrap();
                    cx.new(|_| DecorationScene(Some(image))).into_any_element()
                },
            );
            window.run_until_parked();
            let pixels = window.update(|_, cx| {
                let result = cache.read(cx).item.as_ref().unwrap().get().unwrap();
                if reject {
                    assert!(result.is_err());
                    // Every consumer chooses its existing default cover, and
                    // repeated renders do not restart a rejected load.
                    for _ in 0..3 {
                        assert!(first.image(cx).is_none());
                        assert!(second.image(cx).is_none());
                        assert!(!first.is_animated(cx));
                        assert!(!second.is_animated(cx));
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
                    }
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
            window.update(|_, cx| assert!(cache.read(cx).item.is_none()));
        }
    }

    #[gpui::test]
    fn artwork_budget_changes_retry_shared_covers_and_retire_previous_pixels(
        cx: &mut gpui::TestAppContext,
    ) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("budget.wav");
        cover_wave(&path, &decode::tests::gif());
        let manager = crate::artwork::ArtworkManager::new(directory.path());
        let loaded = manager.load(&crate::artwork::tests::track(path)).unwrap();
        let images = ArtworkImages::default();
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
            assert!(first.image(cx).is_none());
            assert!(second.image(cx).is_none());
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
            assert!(first.image(cx).is_some());
            assert!(second.image(cx).is_some());
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
            assert!(first.image(cx).is_some());
            assert!(second.image(cx).is_some());
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
        window.update(|window, cx| {
            assert!(first.image(cx).is_none());
            assert!(second.image(cx).is_none());
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

    #[gpui::test]
    fn artwork_budget_change_cancels_pending_completion_before_it_can_publish(
        cx: &mut gpui::TestAppContext,
    ) {
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("pending-budget.wav");
        cover_wave(&path, &decode::tests::gif());
        let manager = crate::artwork::ArtworkManager::new(directory.path());
        let loaded = manager.load(&crate::artwork::tests::track(path)).unwrap();
        for worker_finished in [false, true] {
            let images = ArtworkImages::default();
            let view = ArtworkView::new(images.clone());
            let window = cx.add_empty_window();
            let cache = window.update(|_, cx| {
                assert!(images.set_decode_budget(decode::tests::budget(2), cx));
                view.replace(Some(loaded.clone()), cx);
                view.state
                    .borrow()
                    .current
                    .as_ref()
                    .unwrap()
                    .owner
                    .image
                    .clone()
            });
            let old_scene = window.update(|_, cx| view.image(cx).unwrap());
            let (sender, receiver) = futures::channel::oneshot::channel();
            let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
            window.update(|_, cx| {
                let finished = finished.clone();
                cache.update(cx, |image, cx| {
                    image.item = Some(ImageCacheItem::from_future(
                        async move {
                            let result = receiver.await.expect("controlled decode sender is held");
                            finished.store(true, std::sync::atomic::Ordering::SeqCst);
                            result
                        },
                        cx,
                    ));
                });
                assert!(!images.set_decode_budget(decode::tests::budget(2), cx));
            });
            window.run_until_parked();
            assert!(
                !sender.is_canceled(),
                "unchanged budget must keep pending work"
            );

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
            window.update(|_, cx| {
                assert!(images.set_decode_budget(decode::tests::budget(1), cx));
                assert!(cache.read(cx).item.is_none());
            });
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
            // Even a scene built under the old policy now uses the new budget.
            window.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(64.), gpui::px(64.)),
                |_, cx| {
                    cx.new(|_| DecorationScene(Some(old_scene.size_full())))
                        .into_any_element()
                },
            );
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
                assert!(view.image(cx).is_none());
                view.release_all_images(window, cx);
                images.release_all_retired(window, cx);
            });
        }
    }

    struct DecorationScene(Option<Img>);

    impl Render for DecorationScene {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.take().expect("single-use test scene").size_full()
        }
    }

    fn draw_cover(window: &mut gpui::VisualTestContext, cover: &ArtworkView) {
        window.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(64.), gpui::px(64.)),
            |_, cx| {
                let image = cover.image(cx).unwrap();
                cx.new(|_| DecorationScene(Some(image))).into_any_element()
            },
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
    fn artwork_source_animation_uses_one_owner_gate_across_panels_and_suspension(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{
            album_artwork::{AlbumArtwork, AlbumArtworkPanel},
            layout::{Axis, Node, Panel},
        };
        use crate::config::{AlbumArtworkMode, AlbumArtworkOptions, AlbumArtworkStyle, Config};
        use rivu_protocol::model::PlaybackStatus;

        cx.update(|cx| {
            crate::gui::appearance::theme::set_theme(crate::theme::Theme::default(), cx);
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
        });
        std::fs::create_dir_all(".cache").unwrap();
        let directory = tempfile::tempdir_in(".cache").unwrap();
        let path = directory.path().join("paced.wav");
        cover_wave(&path, &decode::tests::gif());
        let track = crate::artwork::tests::track(path);
        let manager = Arc::new(crate::artwork::ArtworkManager::new(directory.path()));
        assert!(manager.load(&track).is_some());
        let images = ArtworkImages::default();
        let owner = cx.update(|cx| {
            let default = cx.new(|cx| Artwork::new(&crate::i18n::Localizer::new("en"), cx));
            cx.new(|_| AlbumArtwork::new(manager, default, images.clone()))
        });
        let sync = |mode, status, fps, visible, cx: &mut gpui::TestAppContext| {
            let config = Config {
                album_artwork_fps: fps,
                ..Config::default()
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
                            kind: "album_artwork".into(),
                            show_tab_bar: true,
                            audio_visualizer: None,
                            album_artwork: Some(AlbumArtworkOptions {
                                mode,
                                style: AlbumArtworkStyle::Plain,
                                ..(&config).into()
                            }),
                        }],
                    })
                    .collect(),
            };
            owner.update(cx, |owner, cx| {
                owner.sync(Some(&track), status, &config, Some(&root), visible, cx);
            });
        };
        sync(
            AlbumArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            cx,
        );
        let window = cx.add_empty_window();
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        let panels =
            [1, 2].map(|id| window.new(|cx| AlbumArtworkPanel::new(id, owner.clone(), cx)));
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
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );

        for mode in [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating] {
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

        window.update(|_, cx| cx.set_reduce_motion(true));
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0
        );
        window.update(|_, cx| cx.set_reduce_motion(false));
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );

        window.deactivate_window();
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );
        draw(window);
        sync(
            AlbumArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        window.update(|window, cx| window.simulate_next_frame(cx));
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "source animation alone stays suspended while unfocused",
        );
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        draw(window);
        assert_eq!(
            window.update(|window, cx| window.simulate_next_frame(cx)),
            1
        );

        // Hide with a native callback outstanding. It may drain once, but cannot
        // restart the old generation while the panels are hidden.
        sync(
            AlbumArtworkMode::Static,
            PlaybackStatus::Paused,
            0,
            true,
            &mut window.cx,
        );
        draw(window);
        sync(
            AlbumArtworkMode::Static,
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
            AlbumArtworkMode::Static,
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
            crate::gui::appearance::theme::set_theme(crate::theme::Theme::default(), cx);
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
        });
        use super::super::{
            album_artwork::{AlbumArtwork, AlbumArtworkPanel},
            layout::{Axis, Node, Panel},
        };
        use crate::config::{AlbumArtworkOptions, AlbumArtworkStyle, Config};
        use rivu_protocol::model::PlaybackStatus;

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
    fn relocation_keeps_latest_path_pending_and_rejects_old_completion_identity() {
        let mut loader = ArtworkLoader::default();
        let first = track(1, "original");
        let mut skipped = first.clone();
        skipped.path = "/moved/first.wav".into();
        let mut latest = skipped.clone();
        latest.path = "/moved/second.wav".into();
        assert!(loader.prepare(Some(&first)).is_some());
        let active = ArtworkIdentity::for_track(&first);
        assert!(loader.prepare(Some(&skipped)).is_none());
        assert!(loader.prepare(Some(&latest)).is_none());
        assert!(!active.matches(&latest));
        let next = loader.finish(&active, Some(&latest)).unwrap();
        assert_eq!(next.path, latest.path);
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
