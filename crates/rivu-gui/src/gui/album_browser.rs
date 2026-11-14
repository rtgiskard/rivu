use rivu_metadata::artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use gpui::{prelude::*, *};

use crate::frontend::FrontendHandle;
use rivu_presentation::track::track_title;
use rivu_protocol::{
    model::{AlbumKey, AlbumSort, ArtworkSource, Command, PlaybackStatus},
    response::QueryRevisions,
};

use super::{
    artwork::{
        ArtworkImageRegistry, ArtworkLoader, ArtworkView, DefaultArtwork, owned_artwork_source,
    },
    components::{
        search::{SearchScope, new_search, set_search_counts},
        track::{TrackText, track_list_row},
    },
    list_interaction::{ContentDrag, ContentMenuRequest, TrackSource},
    scrolling::ScrollViews,
};
use rivu_ui::{
    ButtonTooltip, IconButton, IconButtonDensity, IconButtonGroup, IconButtonStyle, VirtualList,
    caption, column, panel_surface,
    refresh::RefreshWake,
    row,
    search::{SearchEvent, SearchField},
};

mod model;
use model::{AlbumModel, DataChange, Published, Request};

const ALBUM_FADE_FRACTION: f32 = 0.4;

#[derive(Clone, Copy, PartialEq)]
struct AlbumGridLayout {
    columns: usize,
    artwork_size: Pixels,
    metadata_lift: Pixels,
    row_height: Pixels,
    typography_scale: f32,
}

impl AlbumGridLayout {
    fn measured(width: Pixels, rem: Pixels, typography_scale: f32) -> Self {
        let columns = ((width / rem) / 12.).floor().max(1.) as usize;
        let card_width = (width - rem * 0.5 * columns.saturating_sub(1) as f32) / columns as f32;
        let artwork_size = (card_width - rem).max(px(0.));
        let metadata_lift = rem * 0.75 + artwork_size * (ALBUM_FADE_FRACTION - 0.25);
        Self {
            columns,
            artwork_size,
            metadata_lift,
            typography_scale,
            // Cover, three metadata lines, card padding and the grid-row gap;
            // metadata follows the fade's upper edge as its coverage increases.
            row_height: artwork_size + rem * (5.125 + 3.625 * (typography_scale - 1.).max(0.))
                - metadata_lift,
        }
    }
}

struct AlbumCover {
    view: ArtworkView,
    // Only explicit negative results outlive shared-cache eviction, and only
    // for this visibly demanded source. Positive results live in the view.
    negative: Option<ArtworkIdentity>,
}

impl AlbumCover {
    fn new(images: ArtworkImageRegistry) -> Self {
        Self {
            view: ArtworkView::new(images),
            negative: None,
        }
    }

    /// Reconcile a cached completion before deciding whether to schedule a load.
    /// Returns whether the view changed and whether the source is still cold.
    fn sync(
        &mut self,
        track: &ArtworkSource,
        manager: &ArtworkManager,
        cx: &mut App,
    ) -> (bool, bool) {
        if self
            .negative
            .as_ref()
            .is_some_and(|negative| negative.matches(owned_artwork_source(track)))
        {
            return (false, false);
        }
        let cached = manager.cached_result(owned_artwork_source(track));
        let cold = cached.is_none();
        self.negative = cached
            .as_ref()
            .is_some_and(Option::is_none)
            .then(|| ArtworkIdentity::new(owned_artwork_source(track)));
        (self.view.replace(cached.flatten(), cx), cold)
    }

    fn complete(
        &mut self,
        source: &ArtworkSource,
        current: Option<&ArtworkSource>,
        loaded: Option<&LoadedArtwork>,
        cx: &mut App,
    ) -> bool {
        if !current.is_some_and(|current| {
            current.id == source.id
                && current.fingerprint == source.fingerprint
                && current.path == source.path
        }) {
            return false;
        }
        self.negative = loaded
            .is_none()
            .then(|| ArtworkIdentity::new(owned_artwork_source(source)));
        self.view.replace(loaded.cloned(), cx)
    }
}

/// Only viewport/page changes rebuild demand. A completed source addresses its
/// current cards directly; scheduling consumes the remaining viewport once.
#[derive(Default)]
struct AlbumCovers {
    views: BTreeMap<usize, AlbumCover>,
    sources: BTreeMap<i64, Vec<usize>>,
    pending: VecDeque<usize>,
}

impl AlbumCovers {
    fn clear(&mut self, cx: &mut App) {
        self.sources.clear();
        self.pending.clear();
        for cover in std::mem::take(&mut self.views).into_values() {
            cover.view.replace(None, cx);
        }
    }

    fn sync<'a>(
        &mut self,
        demand: Range<usize>,
        source: impl Fn(usize) -> Option<&'a ArtworkSource>,
        manager: &ArtworkManager,
        images: &ArtworkImageRegistry,
        cx: &mut App,
    ) -> bool {
        self.sources.clear();
        self.pending.clear();
        for (_, cover) in self.views.extract_if(.., |index, _| {
            !demand.contains(index) || source(*index).is_none()
        }) {
            cover.view.replace(None, cx);
        }
        let mut changed = false;
        for index in demand {
            let Some(track) = source(index) else {
                continue;
            };
            let cover = self
                .views
                .entry(index)
                .or_insert_with(|| AlbumCover::new(images.clone()));
            let (updated, cold) = cover.sync(track, manager, cx);
            changed |= updated;
            if cold {
                self.pending.push_back(index);
            }
            self.sources.entry(track.id).or_default().push(index);
        }
        changed
    }

    fn prepare<'a>(
        &mut self,
        source: impl Fn(usize) -> Option<&'a ArtworkSource>,
        manager: &ArtworkManager,
        loader: &mut ArtworkLoader,
        cx: &mut App,
    ) -> (Option<ArtworkSource>, bool) {
        let mut changed = false;
        while let Some(&index) = self.pending.front() {
            if let Some(track) = source(index)
                && let Some(cover) = self.views.get_mut(&index)
            {
                let (updated, cold) = cover.sync(track, manager, cx);
                changed |= updated;
                if cold {
                    // Keep this candidate while an obsolete physical worker retires.
                    let next = loader.prepare(owned_artwork_source(track));
                    if next.is_some() {
                        self.pending.pop_front();
                    }
                    return (next, changed);
                }
            }
            self.pending.pop_front();
        }
        (None, changed)
    }

    fn complete<'a>(
        &mut self,
        track: &ArtworkSource,
        loaded: Option<&LoadedArtwork>,
        source: impl Fn(usize) -> Option<&'a ArtworkSource>,
        cx: &mut App,
    ) -> bool {
        let Some(indices) = self.sources.get(&track.id) else {
            return false;
        };
        let mut changed = false;
        for &index in indices {
            if let Some(cover) = self.views.get_mut(&index) {
                changed |= cover.complete(track, source(index), loaded, cx);
            }
        }
        changed
    }
}

/// Sparse absolute rows and viewport-owned covers per panel. Visibility cancels
/// submissions and cover demand, not unchanged album or track data.
pub(super) struct AlbumBrowser {
    panel_id: u64,
    scroll_views: Rc<RefCell<ScrollViews>>,
    handle: FrontendHandle,
    snapshots: Rc<super::feature::SnapshotSource>,
    i18n: Arc<rivu_presentation::i18n::Localizer>,
    localized_text: Rc<HashMap<String, SharedString>>,
    manager: Arc<ArtworkManager>,
    images: ArtworkImageRegistry,
    default_artwork: Entity<DefaultArtwork>,
    model: AlbumModel,
    refresh_wake: RefreshWake,
    loading_text: rivu_ui::loading::LoadingText<()>,
    error: Option<String>,
    dismissed_actions: Option<AlbumKey>,
    covers: AlbumCovers,
    loader: ArtworkLoader,
    demand: Range<usize>,
    grid_layout: AlbumGridLayout,
    grid_scroll: UniformListScrollHandle,
    track_scroll: UniformListScrollHandle,
    grid_anchor: Option<AlbumKey>,
    grid_anchor_index: usize,
    grid_reset: bool,
    album_search: Entity<SearchField>,
    tracks_search: Entity<SearchField>,
    current_track: Option<i64>,
    playback_status: PlaybackStatus,
    _search_subscriptions: Vec<Subscription>,
}

impl EventEmitter<ContentMenuRequest> for AlbumBrowser {}

pub(super) struct AlbumPlayRequest(pub AlbumKey);
impl EventEmitter<AlbumPlayRequest> for AlbumBrowser {}

impl AlbumBrowser {
    pub(super) fn new(
        panel_id: u64,
        context: super::feature::FeatureContext,
        manager: Arc<ArtworkManager>,
        default_artwork: Entity<DefaultArtwork>,
        images: ArtworkImageRegistry,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = context.handle().clone();
        let i18n = context.i18n;
        let scroll_views = context.scroll;
        let localized_text = context.labels;
        let album_search = cx.new(|cx| new_search(&i18n, SearchScope::Albums, cx));
        let tracks_search = cx.new(|cx| new_search(&i18n, SearchScope::Tracks, cx));
        let album_subscription = cx.subscribe(&album_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            if let Some(change) = this.model.set_query(query.clone()) {
                this.reset_grid();
                this.reload(change, cx);
            }
        });
        let tracks_subscription = cx.subscribe(&tracks_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            if let Some(change) = this.model.set_tracks_query(query.clone()) {
                this.track_scroll
                    .scroll_to_item_strict(0, ScrollStrategy::Top);
                this.reload(change, cx);
            }
        });
        Self {
            panel_id,
            scroll_views,
            i18n,
            localized_text,
            handle,
            snapshots: context.snapshots,
            manager,
            images,
            default_artwork,
            model: AlbumModel::default(),
            refresh_wake: RefreshWake::default(),
            loading_text: rivu_ui::loading::LoadingText::new(std::time::Duration::from_millis(150)),
            error: None,
            dismissed_actions: None,
            covers: AlbumCovers::default(),
            loader: ArtworkLoader::default(),
            demand: 0..0,
            grid_layout: AlbumGridLayout::measured(
                px(192.),
                px(16.),
                rivu_ui::theme::font_size_scale(cx),
            ),
            grid_scroll: UniformListScrollHandle::new(),
            track_scroll: UniformListScrollHandle::new(),
            grid_anchor: None,
            grid_anchor_index: 0,
            grid_reset: false,
            album_search,
            tracks_search,
            current_track: None,
            playback_status: PlaybackStatus::Stopped,
            _search_subscriptions: vec![album_subscription, tracks_subscription],
        }
    }

    fn tr(&self, id: &str) -> SharedString {
        self.localized_text
            .get(id)
            .expect("cached localized GUI text")
            .clone()
    }

    fn active_search(&self) -> &Entity<SearchField> {
        if self.model.selected().is_some() {
            &self.tracks_search
        } else {
            &self.album_search
        }
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.model.visible() && self.active_search().read(cx).input_focused(window, cx)
    }

    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_search()
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub(super) fn escape_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.active_search()
            .update(cx, |search, cx| search.escape(window, cx))
    }

    pub(super) fn sync(
        &mut self,
        visible: bool,
        revisions: QueryRevisions,
        page_size: usize,
        current_track: Option<i64>,
        playback_status: PlaybackStatus,
        cx: &mut Context<Self>,
    ) {
        if self.current_track != current_track || self.playback_status != playback_status {
            self.current_track = current_track;
            self.playback_status = playback_status;
            cx.notify();
        }
        if self.model.albums_need_refresh(revisions) || (self.model.visible() && !visible) {
            self.capture_grid_anchor();
        }
        let change = self.model.sync(visible, revisions, page_size);
        let data_changed = change.data.any();
        self.apply_data_change(change.data, cx);
        self.schedule_refresh(cx);
        if change.visibility && !visible {
            self.loading_text.clear();
            self.clear_covers(cx);
        }
        if !visible {
            let mut scroll_views = self.scroll_views.borrow_mut();
            scroll_views.begin_panel(self.panel_id);
            scroll_views.finish_frame();
            self.dismissed_actions = None;
        }
        if visible && (change.visibility || data_changed || change.demand) {
            if change.visibility && self.model.selected().is_none() {
                self.restore_grid_anchor();
            }
            self.request_next(cx);
        }
        if change.visibility || data_changed || change.demand {
            cx.notify();
        }
    }

    fn clear_covers(&mut self, cx: &mut App) {
        self.demand = 0..0;
        self.covers.clear(cx);
    }

    fn apply_data_change(&mut self, change: DataChange, cx: &mut App) {
        self.clear_stale_counts(change, cx);
        if (self.model.selected().is_none() && change.albums)
            || (self.model.selected().is_some() && change.tracks)
        {
            self.loading_text.clear();
        }
    }

    /// Counts stay with retained rows and disappear only with them.
    fn clear_stale_counts(&mut self, change: DataChange, cx: &mut App) {
        if change.albums && self.model.albums().display_total().is_none() {
            self.album_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
        if change.tracks && self.model.tracks().display_total().is_none() {
            self.tracks_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_wake
            .schedule(self.model.refresh_deadline(), cx, |this, now, cx| {
                let expired = this.model.expire_refresh(now);
                this.clear_stale_counts(expired, cx);
                if expired.albums {
                    this.sync_covers(cx);
                }
                this.schedule_refresh(cx);
                expired.any()
            });
    }

    fn reload(&mut self, change: DataChange, cx: &mut Context<Self>) {
        self.loading_text.clear();
        self.clear_covers(cx);
        self.apply_data_change(change, cx);
        self.schedule_refresh(cx);
        if !self.model.visible() {
            return;
        }
        if self.model.selected().is_none() {
            self.restore_grid_anchor();
        }
        self.error = None;
        self.request_next(cx);
        cx.notify();
    }

    fn request_next(&mut self, cx: &mut Context<Self>) {
        let page = self.model.prepare();
        let count = if page.is_none() {
            self.model.prepare_missing_count()
        } else {
            None
        };
        if page.is_none() && count.is_none() {
            return;
        }
        if let Some(page) = &page {
            self.loading_text.begin((), page.generation, Instant::now());
            self.loading_text
                .schedule(cx, |this| &mut this.loading_text);
        }
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let count = if let Some(Request {
                query,
                generation,
                ticket,
            }) = page
            {
                let worker_query = query.clone();
                let response = super::query::run(
                handle.clone(),
                cx,
                move |handle| async move { handle.query_if_current(worker_query, ticket).await },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.loading_text.finish(&(), generation);
                        this.loading_text
                            .schedule(cx, |this| &mut this.loading_text);
                        let revisions = this.snapshots.read().query_revisions(None);
                        if this.model.albums_need_refresh(revisions) {
                            this.capture_grid_anchor();
                        }
                        let data = this.model.timeout(&query, generation, response, revisions);
                        this.apply_data_change(data, cx);
                        this.schedule_refresh(cx);
                        this.sync_covers(cx);
                        cx.notify();
                    });
                },
            )
            .await;
                this.update(cx, |this, cx| {
                    this.loading_text.finish(&(), generation);
                    this.loading_text
                        .schedule(cx, |this| &mut this.loading_text);
                    let revisions = this.snapshots.read().query_revisions(None);
                    if this.model.albums_need_refresh(revisions) {
                        this.capture_grid_anchor();
                    }
                    let completion = this.model.complete_page(
                        &query,
                        generation,
                        response,
                        revisions,
                        this.i18n.text("gl-album-response"),
                    );
                    let published = completion.published;
                    this.apply_completion(completion, cx);
                    if published != Published::None {
                        this.model.prepare_count(&query, generation)
                    } else {
                        None
                    }
                })
                .ok()
                .flatten()
            } else {
                count
            };
            if let Some(count) = count {
                let count_query = count.query.clone();
                let ticket = count.ticket.clone();
                // No page feedback is attached to the auxiliary query's deadline.
                // query_if_current still drains its physical reply after timeout.
                let response = handle.query_if_current(count_query, ticket).await;
                let _ = this.update(cx, |this, cx| {
                    let revisions = this.snapshots.read().query_revisions(None);
                    if this.model.albums_need_refresh(revisions) {
                        this.capture_grid_anchor();
                    }
                    let completion = this.model.complete_count(&count, response, revisions);
                    this.apply_completion(completion, cx);
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.model.retire_worker();
                this.request_next(cx);
            });
        })
        .detach();
    }

    fn apply_completion(&mut self, completion: model::Completion, cx: &mut Context<Self>) {
        // Equal payloads still restore row controls that were disabled while stale.
        let changed =
            completion.changed || completion.data.any() || completion.published != Published::None;
        // Failure or rejection may also withdraw retained cards and their cover demand.
        let albums_changed = completion.data.albums
            || (completion.changed && completion.published != Published::Tracks);
        self.apply_data_change(completion.data, cx);
        if let Some((matches, total)) = completion.counts {
            let search = match completion.published {
                Published::Albums => &self.album_search,
                Published::Tracks => &self.tracks_search,
                Published::None => unreachable!(),
            };
            search.update(cx, |search, cx| {
                set_search_counts(search, matches, total, &self.i18n, cx)
            });
        }
        self.schedule_refresh(cx);
        if albums_changed {
            self.sync_covers(cx);
        } else if completion.published == Published::Albums {
            self.load_next_cover(cx);
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn retry(&mut self, cx: &mut Context<Self>) {
        self.model.retry();
        self.schedule_refresh(cx);
        self.request_next(cx);
        cx.notify();
    }

    fn loading_row(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let error = self.model.albums().error(index);
        column()
            .overflow_hidden()
            .child(
                caption(
                    theme,
                    error.unwrap_or(self.i18n.text("shell-loading")).to_owned(),
                )
                .when(!self.loading_text.show(&(), error.is_some()), |text| {
                    text.invisible()
                }),
            )
            .when(error.is_some(), |view| {
                view.child(
                    IconButton::new(("album-retry", index), "󰑐", self.tr("gl-retry-loading"))
                        .surface(crate::gui::appearance::material::surface(cx))
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            |this, _, cx| this.retry(cx),
                        )
                        .self_start(),
                )
            })
    }

    fn track_loading_row(
        &self,
        index: usize,
        height: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let error = self.model.tracks().error(index);
        rivu_ui::loading::paged_loading_row(
            ("album-track-loading", index),
            error,
            self.loading_text.show(&(), error.is_some()),
            rivu_ui::loading::LoadingRowLabels {
                message: self.tr(if error.is_some() {
                    "shell-loading-failed"
                } else {
                    "shell-loading"
                }),
                retry: self.tr("gl-retry-loading"),
                copy_error: self.tr("shell-copy-message"),
            },
            height,
            cx,
            |this, _, cx| this.retry(cx),
        )
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn open(&mut self, key: AlbumKey, cx: &mut Context<Self>) {
        self.capture_grid_anchor();
        let change = self.model.select(Some(key));
        if change.reset_tracks {
            self.tracks_search.update(cx, |search, cx| search.reset(cx));
            self.track_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.reload(change.data, cx);
    }

    fn reset_grid(&mut self) {
        self.dismissed_actions = None;
        self.reset_grid_scroll();
    }

    fn reset_grid_scroll(&mut self) {
        self.grid_anchor = None;
        self.grid_anchor_index = 0;
        self.grid_reset = true;
        self.grid_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
    }

    fn capture_grid_anchor(&mut self) {
        if self.grid_reset || self.model.selected().is_some() || self.model.albums().is_empty() {
            return;
        }
        let state = self.grid_scroll.0.borrow();
        let grid_row = state.deferred_scroll_to_item.as_ref().map_or_else(
            || {
                // UniformList virtualizes its children; ScrollHandle's child index
                // is not the logical row. Use measured content height instead.
                state.last_item_size.map_or(0, |size| {
                    let height = size.contents.height
                        / self.model.albums().len().div_ceil(self.grid_layout.columns) as f32;
                    (-state.base_handle.offset().y / height).floor().max(0.) as usize
                })
            },
            |target| target.item_index,
        );
        let previous = self
            .grid_anchor
            .as_ref()
            .and_then(|key| self.model.albums().position(|album| &album.key == key))
            .or(Some(self.grid_anchor_index));
        let index = anchored_index(
            previous,
            grid_row,
            self.grid_layout.columns,
            self.model.albums().len(),
        );
        self.grid_anchor = self
            .model
            .albums()
            .get(index)
            .map(|album| album.key.clone());
        self.grid_anchor_index = index;
    }

    fn restore_grid_anchor(&mut self) {
        let index = self
            .grid_anchor
            .as_ref()
            .and_then(|key| self.model.albums().position(|album| &album.key == key))
            .unwrap_or(self.grid_anchor_index)
            .min(self.model.albums().len().saturating_sub(1));
        self.grid_scroll
            .scroll_to_item_strict(index / self.grid_layout.columns, ScrollStrategy::Top);
        self.grid_anchor = self
            .model
            .albums()
            .get(index)
            .map(|album| album.key.clone());
        self.grid_anchor_index = index;
        self.grid_reset = false;
    }

    fn cover_demand(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if !self.model.visible() || self.model.selected().is_some() {
            return;
        }
        let end = range
            .end
            .saturating_mul(self.grid_layout.columns)
            .min(self.model.albums().len());
        let start = range
            .start
            .saturating_mul(self.grid_layout.columns)
            .min(end);
        let changed = self.demand != (start..end);
        self.demand = start..end;
        // Updating demand while a request is pending still records the latest viewport.
        self.model.demand_albums(self.demand.clone());
        if changed {
            self.sync_covers(cx);
        }
        self.request_next(cx);
    }

    fn sync_covers(&mut self, cx: &mut Context<Self>) {
        if !self.model.visible() || self.model.selected().is_some() {
            return;
        }
        let albums = self.model.albums();
        let changed = self.covers.sync(
            self.demand.clone(),
            |index| albums.display(index).map(|album| &album.representative),
            &self.manager,
            &self.images,
            cx,
        );
        if changed {
            cx.notify();
        }
        self.load_next_cover(cx);
    }

    fn load_next_cover(&mut self, cx: &mut Context<Self>) {
        let albums = self.model.albums();
        let (next, changed) = self.covers.prepare(
            |index| albums.get(index).map(|album| &album.representative),
            &self.manager,
            &mut self.loader,
            cx,
        );
        if changed {
            cx.notify();
        }
        if let Some(track) = next {
            let identity = ArtworkIdentity::new(owned_artwork_source(&track));
            let manager = Arc::clone(&self.manager);
            cx.spawn(async move |this, cx| {
                let (track, loaded) = cx
                    .background_executor()
                    .spawn(async move {
                        let loaded = manager.load(owned_artwork_source(&track));
                        (track, loaded)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // The worker result remains leased through direct delivery.
                    // Hidden/obsolete demand can warm the shared cache only.
                    if !this.loader.finish(&identity)
                        || !this.model.visible()
                        || this.model.selected().is_some()
                    {
                        return;
                    }
                    let albums = this.model.albums();
                    let changed = this.covers.complete(
                        &track,
                        loaded.as_ref(),
                        |index| albums.get(index).map(|album| &album.representative),
                        cx,
                    );
                    if changed {
                        cx.notify();
                    }
                    this.load_next_cover(cx);
                });
            })
            .detach();
        }
    }

    pub(super) fn retire_all_images(&mut self, cx: &mut App) {
        self.clear_covers(cx);
    }

    pub(super) fn release_all_images(&mut self, window: &mut Window, cx: &mut App) {
        for cover in self.covers.views.values() {
            cover.view.release_all_images(window, cx);
        }
    }

    fn card(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = rivu_ui::theme::palette(cx);
        let current = self.model.albums().get(index).is_some();
        let Some(album) = self.model.albums().display(index) else {
            return self
                .loading_row(index, cx)
                .flex_1()
                .min_w_0()
                .h_full()
                .p_2()
                .into_any_element();
        };
        let key = album.key.clone();
        let play_key = key.clone();
        let enqueue_key = key.clone();
        let menu_key = key.clone();
        let drag_key = key.clone();
        let drag_snapshots = self.snapshots.clone();
        let hover_key = key.clone();
        let image = self
            .covers
            .views
            .get(&index)
            .and_then(|cover| cover.view.measured_thumbnail(0, cx));
        let directory = key.directory.to_string_lossy().into_owned();
        let title = key.title.clone();
        let drag_label: SharedString = title.clone().into();
        let card_id = gpui::ElementId::NamedChild(
            Arc::new(gpui::ElementId::Path(Arc::from(key.directory.as_path()))),
            drag_label.clone(),
        );
        let cover_fade = |color: u32| {
            linear_gradient(
                0.,
                linear_color_stop(rgb(color), 0.),
                linear_color_stop(rgb(color).alpha(0.), 1.),
            )
        };
        let actions = row()
            .id(("album-actions", index))
            .absolute()
            .right_2()
            .bottom_2()
            .gap_0()
            .when(self.dismissed_actions.as_ref() == Some(&key), |view| {
                view.invisible()
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(rivu_ui::hover_reveal(
                ("album-action-reveal", index),
                "album-card",
                IconButtonGroup::new(IconButtonStyle::Bare, IconButtonDensity::Compact)
                    .surface(crate::gui::appearance::material::surface(cx))
                    .button(
                        IconButton::new(("play-album", index), "", self.tr("gl-play-album"))
                            .surface(crate::gui::appearance::material::surface(cx)),
                        cx,
                        move |this, window, cx| {
                            this.dismissed_actions = Some(play_key.clone());
                            cx.notify();
                            window.blur(cx);
                            cx.emit(AlbumPlayRequest(play_key.clone()));
                        },
                    )
                    .button(
                        IconButton::new(("enqueue-album", index), "󰐕", self.tr("gl-enqueue-album"))
                            .surface(crate::gui::appearance::material::surface(cx)),
                        cx,
                        move |this, window, cx| {
                            this.dismissed_actions = Some(enqueue_key.clone());
                            cx.notify();
                            window.blur(cx);
                            this.send(
                                Command::EnqueueAlbum {
                                    key: enqueue_key.clone(),
                                },
                                cx,
                            )
                        },
                    )
                    .into_div(),
            ));
        column()
            .id(card_id)
            .group("album-card")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_2()
            .gap_0()
            .when(current, |view| {
                view.cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if !hovered && this.dismissed_actions.as_ref() == Some(&hover_key) {
                            this.dismissed_actions = None;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if event.click_count() == 2 {
                            this.open(key.clone(), cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(ContentMenuRequest {
                                source: TrackSource::Album {
                                    key: menu_key.clone(),
                                    revision: this.snapshots.read().library.album_revision,
                                },
                                position: event.position,
                                play: Some(Command::PlayAlbum {
                                    key: menu_key.clone(),
                                }),
                            });
                        }),
                    )
                    .on_drag(ContentDrag::pending(), move |drag, _, window, cx| {
                        drag.initialize(TrackSource::Album {
                            key: drag_key.clone(),
                            revision: drag_snapshots.read().library.album_revision,
                        });
                        rivu_ui::begin_list_drag(window, cx);
                        cx.new(|_| rivu_ui::ListDragPreview {
                            label: drag_label.clone(),
                        })
                    })
            })
            .rounded_md()
            .hover(|style| style.bg(rgb(theme.highlight)))
            .overflow_hidden()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(self.grid_layout.artwork_size)
                    .flex_shrink_0()
                    .map(|view| match image {
                        Some(image) => view.child(div().absolute().size_full().child(image)),
                        None => view.child(self.default_artwork.clone()),
                    })
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .w_full()
                            .h(self.grid_layout.artwork_size * ALBUM_FADE_FRACTION)
                            .bg(cover_fade(theme.panel))
                            .group_hover("album-card", move |style| {
                                style.bg(cover_fade(theme.highlight))
                            }),
                    ),
            )
            .child(
                div()
                    .id(("album-title", index))
                    .relative()
                    .mt(-self.grid_layout.metadata_lift)
                    .w_full()
                    .h(rems(1.25 * self.grid_layout.typography_scale.max(1.)))
                    .line_height(rems(1.25))
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(rgb(theme.text))
                    .text_center()
                    .truncate()
                    .child(title.clone())
                    .tooltip(move |_, cx| {
                        cx.new(|_| ButtonTooltip {
                            text: format!("{title}\n{directory}").into(),
                        })
                        .into()
                    }),
            )
            .child(
                caption(
                    theme,
                    if album.artist.trim().is_empty() {
                        "-".into()
                    } else {
                        album.artist.clone()
                    },
                )
                .w_full()
                .text_center()
                .truncate()
                .h(rems(1.125 * self.grid_layout.typography_scale.max(1.)))
                .line_height(rems(1.125))
                .flex_shrink_0(),
            )
            .child(
                caption(
                    theme,
                    self.i18n
                        .format("gl-track-count", &[("count", album.track_count.into())]),
                )
                .w_full()
                .text_center()
                .h(rems(1.25 * self.grid_layout.typography_scale.max(1.)))
                .line_height(rems(1.25))
                .truncate()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0(),
            )
            .when(current, |view| view.child(actions))
            .into_any_element()
    }
}

// Reflow can place earlier cards before the anchor in its new row. Keep the
// identity until the user actually scrolls to a different row.
fn anchored_index(previous: Option<usize>, row: usize, columns: usize, count: usize) -> usize {
    previous
        .filter(|index| *index / columns == row)
        .unwrap_or_else(|| row.saturating_mul(columns))
        .min(count.saturating_sub(1))
}

impl Render for AlbumBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scroll_views.borrow_mut().begin_panel(self.panel_id);
        let theme = rivu_ui::theme::palette(cx);
        let total = self.model.display_total();
        let header;
        if let Some(key) = self.model.selected() {
            let play_key = key.clone();
            let enqueue_key = key.clone();
            let menu_key = key.clone();
            let heading = row()
                .gap_1()
                .child(
                    IconButton::new("albums-back", "", self.tr("gl-back-albums"))
                        .surface(crate::gui::appearance::material::surface(cx))
                        .build(
                            IconButtonStyle::Quiet,
                            IconButtonDensity::Regular,
                            cx,
                            |this, _, cx| {
                                let change = this.model.select(None);
                                this.reload(change.data, cx);
                            },
                        ),
                )
                .child(
                    div()
                        .id("album-selected-title")
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(ContentMenuRequest {
                                    source: TrackSource::Album {
                                        key: menu_key.clone(),
                                        revision: this.snapshots.read().library.album_revision,
                                    },
                                    position: event.position,
                                    play: Some(Command::PlayAlbum {
                                        key: menu_key.clone(),
                                    }),
                                });
                            }),
                        )
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme.text))
                        .child(key.title.clone()),
                );
            let actions = rivu_ui::panel_header_actions()
                .child(self.tracks_search.clone())
                .when(self.model.retired(), |actions| {
                    actions.child(
                        IconButton::new("album-retired-retry", "󰑐", self.tr("gl-retry-loading"))
                            .surface(crate::gui::appearance::material::surface(cx))
                            .build(
                                IconButtonStyle::Surface,
                                IconButtonDensity::Regular,
                                cx,
                                |this, _, cx| this.retry(cx),
                            ),
                    )
                })
                .child(
                    IconButton::new("album-play-all", "", self.tr("gl-play-album"))
                        .surface(crate::gui::appearance::material::surface(cx))
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            move |_, _, cx| {
                                cx.emit(AlbumPlayRequest(play_key.clone()));
                            },
                        ),
                )
                .child(
                    IconButton::new("album-add-all", "󰐕", self.tr("gl-enqueue-album"))
                        .surface(crate::gui::appearance::material::surface(cx))
                        .build(
                            IconButtonStyle::Surface,
                            IconButtonDensity::Regular,
                            cx,
                            move |this, _, cx| {
                                this.send(
                                    Command::EnqueueAlbum {
                                        key: enqueue_key.clone(),
                                    },
                                    cx,
                                )
                            },
                        ),
                );
            header = rivu_ui::panel_heading_row(heading, actions);
        } else {
            let heading = rivu_ui::list_panel_header(
                theme,
                self.tr("shell-panel-albums"),
                total.map_or_else(
                    || "—".to_owned(),
                    |count| {
                        self.i18n
                            .format("gl-entry-count", &[("count", count.into())])
                    },
                ),
            );
            let actions = rivu_ui::panel_header_actions()
                .child(self.album_search.clone())
                .when(self.model.retired(), |actions| {
                    actions.child(
                        IconButton::new("albums-retired-retry", "󰑐", self.tr("gl-retry-loading"))
                            .surface(crate::gui::appearance::material::surface(cx))
                            .build(
                                IconButtonStyle::Surface,
                                IconButtonDensity::Regular,
                                cx,
                                |this, _, cx| this.retry(cx),
                            ),
                    )
                })
                .child(
                    IconButton::new(
                        "albums-sort",
                        match self.model.sort() {
                            AlbumSort::Title => "󰉿",
                            AlbumSort::Artist => "󰀄",
                        },
                        match self.model.sort() {
                            AlbumSort::Title => self.tr("gl-album-sort-title"),
                            AlbumSort::Artist => self.tr("gl-album-sort-artist"),
                        },
                    )
                    .surface(crate::gui::appearance::material::surface(cx))
                    .build(
                        IconButtonStyle::Surface,
                        IconButtonDensity::Regular,
                        cx,
                        |this, _, cx| {
                            let change = this.model.toggle_sort();
                            this.reset_grid();
                            this.reload(change, cx);
                        },
                    ),
                );
            header = rivu_ui::panel_heading_row(heading, actions);
        }
        let generation = self.model.generation();
        let track_demand_owner = cx.weak_entity();
        let content = if self.model.selected().is_some() {
            if self.model.tracks().is_empty() {
                column()
                    .child(caption(
                        theme,
                        self.tr(if self.model.tracks_query().is_some() && total != Some(0) {
                            "gl-album-tracks-no-matches"
                        } else {
                            "gl-album-no-tracks"
                        }),
                    ))
                    .into_any_element()
            } else {
                if self.model.visible() {
                    self.scroll_views
                        .borrow_mut()
                        .register_list(("album-tracks", self.panel_id), &self.track_scroll);
                }
                VirtualList::new(
                    "album-tracks",
                    self.model.tracks().len(),
                    &self.track_scroll,
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        let row_height = rivu_ui::list_row_height(window, cx);
                        range
                            .map(|index| {
                                let current = this.model.tracks().get(index).is_some();
                                let Some(track) = this.model.tracks().display(index) else {
                                    return this
                                        .track_loading_row(index, row_height, cx)
                                        .into_any_element();
                                };
                                let id = track.id;
                                let drag_label: SharedString =
                                    track_title(&track.title, &track.path, track.cue_number)
                                        .into_owned()
                                        .into();
                                let playing = this.current_track == Some(id)
                                    && this.playback_status == PlaybackStatus::Playing;
                                track_list_row(
                                    theme,
                                    rivu_ui::list_row(
                                        theme,
                                        ("album-track", id as u64),
                                        false,
                                        row_height,
                                        crate::gui::appearance::material::surface(cx),
                                    ),
                                    TrackText {
                                        album: None,
                                        ..TrackText::from(track)
                                    },
                                    index,
                                    playing,
                                    &this.i18n,
                                )
                                .group("album-track-row")
                                .relative()
                                .gap_0()
                                .when(current, |row| {
                                    row.on_click(cx.listener(
                                        move |this, event: &ClickEvent, _, cx| {
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            cx.emit(ContentMenuRequest {
                                                source: TrackSource::Tracks(vec![id]),
                                                position: event.position,
                                                play: Some(Command::Play { track_id: id }),
                                            });
                                        }),
                                    )
                                    .on_drag(
                                        ContentDrag::new(TrackSource::Tracks(vec![id])),
                                        move |_, _, window, cx| {
                                            rivu_ui::begin_list_drag(window, cx);
                                            cx.new(|_| rivu_ui::ListDragPreview {
                                                label: drag_label.clone(),
                                            })
                                        },
                                    )
                                })
                                .when(track.missing, |row| {
                                    row.child(caption(theme, this.tr("gl-missing")))
                                })
                                .when(current, |row| {
                                    row.child(
                                        rivu_ui::hover_actions(
                                            theme,
                                            ("album-track-actions", id as u64),
                                            "album-track-row",
                                            false,
                                        )
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .on_click(|_, _, cx| cx.stop_propagation())
                                        .child(
                                            IconButtonGroup::new(
                                                IconButtonStyle::Quiet,
                                                IconButtonDensity::Compact,
                                            )
                                            .surface(crate::gui::appearance::material::surface(cx))
                                            .button(
                                                IconButton::new(
                                                    ("album-track-play", id as u64),
                                                    "",
                                                    this.tr("gl-play-track"),
                                                )
                                                .surface(
                                                    crate::gui::appearance::material::surface(cx),
                                                ),
                                                cx,
                                                move |this, _, cx| {
                                                    this.send(Command::Play { track_id: id }, cx)
                                                },
                                            )
                                            .button(
                                                IconButton::new(
                                                    ("album-track-add", id as u64),
                                                    "󰐕",
                                                    this.tr("gl-enqueue-track"),
                                                )
                                                .surface(
                                                    crate::gui::appearance::material::surface(cx),
                                                ),
                                                cx,
                                                move |this, _, cx| {
                                                    this.send(
                                                        Command::Enqueue {
                                                            track_ids: vec![id],
                                                        },
                                                        cx,
                                                    )
                                                },
                                            )
                                            .into_div(),
                                        ),
                                    )
                                })
                                .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .on_viewport_deferred(track_demand_owner, move |this, range, cx| {
                    if this.model.demand_tracks(range, generation) {
                        this.request_next(cx);
                    }
                })
                .size_full()
                .scrollbar("album-tracks-scrollbar")
                .into_any_element()
            }
        } else if self.model.albums().is_empty() {
            column()
                .child(caption(
                    theme,
                    if self.model.query().is_some() && total != Some(0) {
                        self.tr("gl-albums-no-matches")
                    } else {
                        self.tr("gl-albums-empty")
                    },
                ))
                .into_any_element()
        } else {
            if self.model.visible() {
                self.scroll_views
                    .borrow_mut()
                    .register_list(("album-grid", self.panel_id), &self.grid_scroll);
            }
            let layout = self.grid_layout;
            let columns = layout.columns;
            let generation = self.model.generation();
            let demand_owner = cx.weak_entity();
            let measure_owner = cx.weak_entity();
            VirtualList::new(
                "album-grid",
                self.model.albums().len().div_ceil(columns),
                &self.grid_scroll,
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    range
                        .map(|grid_row| {
                            let start = grid_row * columns;
                            let end = (start + columns).min(this.model.albums().len());
                            let mut cards = div()
                                .grid()
                                .grid_cols(columns as u16)
                                .gap_2()
                                .w_full()
                                .h(layout.row_height)
                                .pb_2();
                            for index in start..end {
                                cards = cards.child(this.card(index, cx));
                            }
                            cards
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .on_viewport(move |range, _, cx| {
                let owner = demand_owner.clone();
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |this, cx| {
                        if generation == this.model.generation() && layout == this.grid_layout {
                            this.cover_demand(range, cx);
                        }
                    });
                });
            })
            .size_full()
            .scrollbar_with("album-grid-scrollbar", |list| {
                div()
                    .relative()
                    .size_full()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let measured = AlbumGridLayout::measured(
                                    bounds.size.width,
                                    window.rem_size(),
                                    rivu_ui::theme::font_size_scale(cx),
                                );
                                if measured == layout {
                                    return;
                                }
                                let owner = measure_owner.clone();
                                cx.defer(move |cx| {
                                    let _ = owner.update(cx, |this, cx| {
                                        if this.grid_layout != measured {
                                            this.capture_grid_anchor();
                                            this.grid_layout = measured;
                                            this.restore_grid_anchor();
                                            cx.notify();
                                        }
                                    });
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(list)
            })
            .into_any_element()
        };
        self.scroll_views.borrow_mut().finish_frame();
        panel_surface("albums-browser")
            .child(header)
            .when_some(self.error.clone(), |view, error| {
                view.child(caption(theme, error))
            })
            .child(div().flex_1().min_h_0().min_w_0().child(content))
    }
}

#[cfg(test)]
mod tests {
    use super::{AlbumCover, AlbumCovers, anchored_index};
    use crate::gui::artwork::{ArtworkImageRegistry, ArtworkLoader, owned_artwork_source};
    use rivu_metadata::artwork::{ArtworkIdentity, ArtworkManager, test_support::tagged_wav};
    use rivu_protocol::model::ArtworkSource;

    fn cover_source(path: std::path::PathBuf) -> ArtworkSource {
        ArtworkSource {
            id: 1,
            path,
            fingerprint: None,
        }
    }

    #[gpui::test]
    fn stats_refresh_retains_cover_demand_and_in_flight_completion(cx: &mut gpui::TestAppContext) {
        use rivu_protocol::{
            model::{AlbumKey, AlbumSummary, Page},
            response::{QueryResponse, QueryRevisions, ViewResponse},
        };

        for positive in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("cover.wav");
            if positive {
                tagged_wav(&path);
            }
            let manager = ArtworkManager::new(directory.path());
            let images = ArtworkImageRegistry::default();
            let mut model = super::AlbumModel::default();
            let revisions = QueryRevisions::default();
            model.sync(true, revisions, 2);
            let cards = model.prepare().unwrap();
            let card = AlbumSummary {
                key: AlbumKey {
                    directory: directory.path().to_owned(),
                    title: "Album".into(),
                },
                artist: "Artist".into(),
                track_count: 1,
                representative: cover_source(path),
            };
            model.complete_page(
                &cards.query,
                cards.generation,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::Albums(Page {
                        total: 1,
                        rows: vec![card.clone()],
                    })),
                },
                revisions,
                "wrong response",
            );
            model.retire_worker();
            let mut covers = AlbumCovers::default();
            let mut loader = ArtworkLoader::default();
            let active = cx.update(|cx| {
                let source = |index| model.albums().get(index).map(|album| &album.representative);
                covers.sync(0..1, source, &manager, &images, cx);
                covers.prepare(source, &manager, &mut loader, cx).0.unwrap()
            });
            let sources = covers.sources.clone();
            let current = QueryRevisions {
                library: 1,
                ..revisions
            };
            assert!(!model.albums_need_refresh(current));
            let change = model.sync(true, current, 2);
            assert!(!change.data.any());
            assert!(!change.visibility && !change.demand);
            assert_eq!(model.albums().get(0), Some(&card));
            assert!(model.prepare().is_none());
            assert_eq!(covers.sources, sources);
            assert!(covers.pending.is_empty());
            assert!(loader.prepare(owned_artwork_source(&active)).is_none());
            let loaded = manager.load(owned_artwork_source(&active));
            assert_eq!(loaded.is_some(), positive);
            assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&active))));
            cx.update(|cx| {
                let source = |index| model.albums().get(index).map(|album| &album.representative);
                assert_eq!(
                    covers.complete(&active, loaded.as_ref(), source, cx),
                    positive
                );
                assert!(!covers.sync(0..1, source, &manager, &images, cx));
                assert!(
                    covers
                        .prepare(source, &manager, &mut loader, cx)
                        .0
                        .is_none()
                );
                assert_eq!(covers.views[&0].view.image(cx).is_some(), positive);
                assert_eq!(covers.views[&0].negative.is_some(), !positive);
                covers.clear(cx);
                assert!(covers.views.is_empty());
                assert!(covers.sources.is_empty());
                assert!(covers.pending.is_empty());
            });
        }
    }

    #[gpui::test]
    fn removed_presentation_releases_cover_demand(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        tagged_wav(&path);
        let source = cover_source(path);
        let manager = ArtworkManager::new(directory.path());
        let loaded = manager.load(owned_artwork_source(&source)).unwrap();
        let mut covers = AlbumCovers::default();
        cx.update(|cx| {
            let images = ArtworkImageRegistry::default();
            covers.sync(0..1, |_| Some(&source), &manager, &images, cx);
            assert!(covers.views[&0].view.image(cx).is_some());
            // Failed/retired pages remove display rows without changing the viewport.
            covers.sync(0..1, |_| None, &manager, &images, cx);
            assert!(covers.views.is_empty());
            assert!(covers.sources.is_empty());
            assert!(covers.pending.is_empty());
        });
        drop(loaded);
    }

    #[gpui::test]
    fn pending_album_tracks_expose_failure_copy_and_retry(cx: &mut gpui::TestAppContext) {
        use crate::gui::feature::{FeatureContext, SnapshotSource};
        use crate::test_support::{ConfigFixture, CoreRuntime};
        use gpui::{prelude::*, *};
        use rivu_protocol::response::{QueryError, QueryResponse};

        struct LoadingTrack(Entity<super::AlbumBrowser>);

        impl Render for LoadingTrack {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.0.update(cx, |browser, cx| {
                    browser.track_loading_row(0, px(24.), cx).w(px(240.))
                })
            }
        }

        cx.executor().allow_parking();
        cx.update(|cx| {
            crate::gui::appearance::theme::set_theme(
                rivu_presentation::theme::Theme::default(),
                cx,
            );
            crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
            rivu_ui::input::Input::init(cx);
        });
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        rivu_format::config::Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Default::default()
        }
        .save_fixture(&config)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config).unwrap();
        let i18n = std::sync::Arc::new(rivu_presentation::i18n::Localizer::new("en-US"));
        let context = FeatureContext {
            snapshots: std::rc::Rc::new(SnapshotSource::new(runtime.handle.clone())),
            labels: std::rc::Rc::new(
                i18n.fixed_texts()
                    .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
                    .collect(),
            ),
            i18n: i18n.clone(),
            scroll: Default::default(),
        };
        let default_artwork = cx.new(|cx| super::DefaultArtwork::new(&i18n, cx));
        let browser = cx.new(|cx| {
            super::AlbumBrowser::new(
                1,
                context,
                std::sync::Arc::new(ArtworkManager::new(directory.path())),
                default_artwork,
                ArtworkImageRegistry::default(),
                cx,
            )
        });
        let revisions = runtime.handle.gui_snapshot().query_revisions(None);
        let pending = browser.update(cx, |browser, _| {
            browser.model.sync(true, revisions, 2);
            browser.model.select(Some(rivu_protocol::model::AlbumKey {
                directory: directory.path().join("album"),
                title: "Album".into(),
            }));
            browser.model.prepare().unwrap()
        });
        let window = cx.add_window(|_, _| LoadingTrack(browser.clone()));
        let handle = window.into();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();

        let error = "Album page failed: Unicode 路径\nFull diagnostic";
        browser.update(cx, |browser, _| {
            browser.model.timeout(
                &pending.query,
                pending.generation,
                QueryResponse {
                    revisions,
                    result: Err(QueryError::Message(error.into())),
                },
                revisions,
            );
        });
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.focus_next(cx);
        })
        .unwrap();
        for retry in [false, true] {
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                let keystroke = Keystroke::parse("enter").unwrap();
                window.dispatch_event(
                    KeyDownEvent {
                        keystroke: keystroke.clone(),
                        is_held: false,
                        prefer_character_input: false,
                    }
                    .to_platform_input(),
                    cx,
                );
                window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), cx);
                window.focus_next(cx);
            })
            .unwrap();
            assert_eq!(
                browser.read_with(cx, |browser, _| browser.model.tracks().error(0).is_none()),
                retry,
            );
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(error),
            );
        }
        // Retry must leave the expired physical worker in charge of retirement.
        browser.update(cx, |browser, _| assert!(browser.model.prepare().is_none()));
        cx.update_window(handle, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    #[gpui::test]
    fn positive_cover_completion_survives_shared_cache_pressure(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        tagged_wav(&path);
        let manager = ArtworkManager::new(directory.path());
        let mut source = cover_source(path);
        let mut held = Vec::new();
        for id in 1..=64 {
            source.id = id;
            held.push(manager.load(owned_artwork_source(&source)).unwrap());
        }
        source.id = 65;
        // The worker's result must stay leased while the GUI callback is queued.
        let completed = manager.load(owned_artwork_source(&source)).unwrap();
        let mut pressure = source.clone();
        pressure.id = 66;
        drop(manager.load(owned_artwork_source(&pressure)));

        let mut cover = AlbumCover::new(ArtworkImageRegistry::default());
        cx.update(|cx| {
            assert!(cover.complete(&source, Some(&source), Some(&completed), cx));
            assert!(cover.view.image(cx).is_some());
            assert!(cover.negative.is_none());
        });
        drop(completed);
        drop(held);
        for id in 67..140 {
            pressure.id = id;
            drop(manager.load(owned_artwork_source(&pressure)));
        }
        assert!(
            manager
                .cached_result(owned_artwork_source(&source))
                .is_some()
        );
        cx.update(|cx| {
            assert_eq!(cover.sync(&source, &manager, cx), (false, false));
            assert!(cover.view.image(cx).is_some());
        });
    }

    #[gpui::test]
    fn shared_completions_reach_waiting_cards_without_new_loads(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        tagged_wav(&path);
        let first = cover_source(path.clone());
        let mut second = cover_source(path);
        second.id += 1;
        let mut negative = cover_source(directory.path().join("missing.wav"));
        negative.id += 2;
        let sources = [first, second, negative];
        let manager = ArtworkManager::new(directory.path());
        let mut covers = AlbumCovers::default();
        let mut loader = ArtworkLoader::default();
        cx.update(|cx| {
            covers.sync(
                0..sources.len(),
                |index| sources.get(index),
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
        });
        let active = cx
            .update(|cx| {
                covers
                    .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                    .0
            })
            .unwrap();
        // Another consumer finishes both waiting sources while this panel loads A.
        let external = manager.load(owned_artwork_source(&sources[1])).unwrap();
        assert!(manager.load(owned_artwork_source(&sources[2])).is_none());
        let completed = manager.load(owned_artwork_source(&active)).unwrap();
        assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&active))));
        cx.update(|cx| {
            covers.complete(&active, Some(&completed), |index| sources.get(index), cx);
            let (next, changed) =
                covers.prepare(|index| sources.get(index), &manager, &mut loader, cx);
            assert!(next.is_none());
            assert!(changed);
            assert!(covers.views[&0].view.image(cx).is_some());
            assert!(covers.views[&1].view.image(cx).is_some());
            assert!(covers.views[&2].view.image(cx).is_none());
            assert!(
                covers.views[&2]
                    .negative
                    .as_ref()
                    .unwrap()
                    .matches(owned_artwork_source(&sources[2]))
            );
        });
        drop(external);
        let mut pressure = sources[2].clone();
        for id in 10..106 {
            pressure.id = id;
            drop(manager.load(owned_artwork_source(&pressure)));
        }
        assert!(
            manager
                .cached_result(owned_artwork_source(&sources[2]))
                .is_none()
        );
        cx.update(|cx| {
            covers.sync(
                0..sources.len(),
                |index| sources.get(index),
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
            assert!(
                covers
                    .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                    .0
                    .is_none()
            );
            assert!(covers.views[&1].view.image(cx).is_some());
            assert!(
                covers.views[&2]
                    .negative
                    .as_ref()
                    .unwrap()
                    .matches(owned_artwork_source(&sources[2]))
            );
        });
    }

    #[gpui::test]
    fn completed_negative_covers_beyond_shared_capacity_converge(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let sources: Vec<_> = (1..=96)
            .map(|id| {
                let mut source = cover_source(directory.path().join(format!("missing-{id}.wav")));
                source.id = id;
                source
            })
            .collect();
        let mut covers = AlbumCovers::default();
        let mut loader = ArtworkLoader::default();
        cx.update(|cx| {
            covers.sync(
                0..sources.len(),
                |index| sources.get(index),
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
        });
        for source in &sources {
            let next = cx
                .update(|cx| {
                    covers
                        .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                        .0
                })
                .unwrap();
            assert!(
                ArtworkIdentity::new(owned_artwork_source(source))
                    .matches(owned_artwork_source(&next))
            );
            assert!(
                cx.update(|cx| covers
                    .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                    .0)
                    .is_none()
            );
            let completed = manager.load(owned_artwork_source(&next));
            assert!(completed.is_none());
            assert_eq!(
                manager.cached_result(owned_artwork_source(source)),
                Some(None)
            );
            assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&next))));
            cx.update(|cx| {
                covers.complete(&next, completed.as_ref(), |index| sources.get(index), cx);
            });
        }
        assert!(
            manager
                .cached_result(owned_artwork_source(&sources[0]))
                .is_none()
        );
        assert!(
            cx.update(|cx| covers
                .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                .0)
                .is_none()
        );
        for (index, source) in sources.iter().enumerate() {
            let cover = covers.views.get_mut(&index).unwrap();
            cx.update(|cx| {
                assert_eq!(cover.sync(source, &manager, cx), (false, false));
                assert!(cover.view.image(cx).is_none());
            });
        }
    }

    #[gpui::test]
    fn obsolete_or_relocated_negative_completion_cannot_claim_current_demand(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let source = cover_source(directory.path().join("missing.wav"));
        let negative = manager.load(owned_artwork_source(&source));
        assert!(negative.is_none());
        assert_eq!(
            manager.cached_result(owned_artwork_source(&source)),
            Some(None)
        );
        let mut current = source.clone();
        current.path = directory.path().join("available.wav");
        tagged_wav(&current.path);
        let mut cover = AlbumCover::new(ArtworkImageRegistry::default());
        let mut replaced = source.clone();
        replaced.fingerprint = Some("replacement".into());
        cx.update(|cx| {
            assert!(!cover.complete(&source, None, negative.as_ref(), cx));
            assert!(cover.negative.is_none());
            assert!(!cover.complete(&source, Some(&replaced), negative.as_ref(), cx));
            assert!(cover.negative.is_none());
            assert!(!cover.complete(&source, Some(&current), negative.as_ref(), cx));
            assert!(cover.negative.is_none());
            cover.complete(&source, Some(&source), negative.as_ref(), cx);
        });
        cx.update(|cx| assert!(cover.sync(&current, &manager, cx).1));
        let recovered = manager.load(owned_artwork_source(&current)).unwrap();
        cx.update(|cx| {
            assert!(cover.complete(&current, Some(&current), Some(&recovered), cx));
            assert!(cover.view.image(cx).is_some());
            assert!(cover.negative.is_none());
        });
    }

    #[gpui::test]
    fn one_source_completion_publishes_to_every_current_card(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        tagged_wav(&path);
        let first = cover_source(path);
        let mut second = cover_source(directory.path().join("missing.wav"));
        second.id = first.id + 1;
        let sources = [first.clone(), second.clone(), first.clone()];
        let manager = ArtworkManager::new(directory.path());
        let mut covers = AlbumCovers::default();
        let mut loader = ArtworkLoader::default();
        cx.update(|cx| {
            covers.sync(
                0..sources.len(),
                |index| sources.get(index),
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
        });
        let next = cx
            .update(|cx| {
                covers
                    .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                    .0
            })
            .unwrap();
        assert!(
            ArtworkIdentity::new(owned_artwork_source(&first)).matches(owned_artwork_source(&next))
        );
        let completed = manager.load(owned_artwork_source(&next)).unwrap();
        assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&next))));
        cx.update(|cx| {
            assert!(covers.complete(&next, Some(&completed), |index| sources.get(index), cx));
            assert!(covers.views[&0].view.image(cx).is_some());
            assert!(covers.views[&1].view.image(cx).is_none());
            assert!(covers.views[&2].view.image(cx).is_some());
        });
        let next = cx
            .update(|cx| {
                covers
                    .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                    .0
            })
            .unwrap();
        assert!(
            ArtworkIdentity::new(owned_artwork_source(&second))
                .matches(owned_artwork_source(&next))
        );
        let completed = manager.load(owned_artwork_source(&next));
        assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&next))));
        cx.update(|cx| {
            covers.complete(&next, completed.as_ref(), |index| sources.get(index), cx);
        });
        assert!(
            cx.update(|cx| covers
                .prepare(|index| sources.get(index), &manager, &mut loader, cx)
                .0)
                .is_none()
        );
    }

    #[gpui::test]
    fn changed_viewport_waits_for_retirement_and_rejects_old_source(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let old = cover_source(directory.path().join("missing.wav"));
        let mut discarded = old.clone();
        discarded.id += 1;
        let previous = [old.clone(), discarded];
        let mut current = old.clone();
        current.path = directory.path().join("available.wav");
        tagged_wav(&current.path);
        let manager = ArtworkManager::new(directory.path());
        let mut covers = AlbumCovers::default();
        let mut loader = ArtworkLoader::default();
        cx.update(|cx| {
            covers.sync(
                0..2,
                |index| previous.get(index),
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
        });
        let active = cx
            .update(|cx| {
                covers
                    .prepare(|index| previous.get(index), &manager, &mut loader, cx)
                    .0
            })
            .unwrap();
        assert!(
            ArtworkIdentity::new(owned_artwork_source(&old)).matches(owned_artwork_source(&active))
        );
        // A page can arrive for the latest viewport while the old load is physical.
        cx.update(|cx| {
            covers.sync(
                2..3,
                |_| None,
                &manager,
                &ArtworkImageRegistry::default(),
                cx,
            );
        });
        assert!(
            cx.update(|cx| covers.prepare(|_| None, &manager, &mut loader, cx).0)
                .is_none()
        );
        let latest = |index| (index == 2).then_some(&current);
        cx.update(|cx| {
            covers.sync(2..3, latest, &manager, &ArtworkImageRegistry::default(), cx);
        });
        assert!(
            cx.update(|cx| covers.prepare(latest, &manager, &mut loader, cx).0)
                .is_none()
        );
        let negative = manager.load(owned_artwork_source(&active));
        assert!(negative.is_none());
        assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&active))));
        cx.update(|cx| {
            assert!(!covers.complete(&active, negative.as_ref(), latest, cx));
            assert!(covers.views[&2].negative.is_none());
            assert!(covers.views[&2].view.image(cx).is_none());
        });
        let next = cx
            .update(|cx| covers.prepare(latest, &manager, &mut loader, cx).0)
            .unwrap();
        assert!(
            ArtworkIdentity::new(owned_artwork_source(&current))
                .matches(owned_artwork_source(&next))
        );
        let completed = manager.load(owned_artwork_source(&next)).unwrap();
        assert!(loader.finish(&ArtworkIdentity::new(owned_artwork_source(&next))));
        cx.update(|cx| {
            assert!(covers.complete(&next, Some(&completed), latest, cx));
            assert!(covers.views[&2].view.image(cx).is_some());
        });
        assert!(
            cx.update(|cx| covers.prepare(latest, &manager, &mut loader, cx).0)
                .is_none()
        );
    }

    #[test]
    fn reflow_retains_anchor_until_user_scrolls() {
        let anchor = anchored_index(Some(8), 2, 3, 24);
        assert_eq!(anchor, 8);
        let anchor = anchored_index(Some(anchor), 2, 4, 24);
        assert_eq!(anchor, 8);
        assert_eq!(anchored_index(Some(anchor), 3, 4, 24), 12);
        assert_eq!(anchored_index(Some(22), 7, 3, 10), 9);
    }
}
