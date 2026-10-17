use std::{collections::BTreeMap, ops::Range, path::PathBuf, sync::Arc, time::Instant};

use gpui::{prelude::*, *};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager},
    core::CoreHandle,
    model::{AlbumKey, AlbumSort, AlbumSummary, Command, LibraryRow, PlaybackStatus, Query, Track},
    projection::QueryProjection,
    response::{QueryRevisions, ViewResponse},
};

use super::{
    artwork::{Artwork, ArtworkImages, ArtworkLoader, ArtworkView},
    components::{
        ButtonTooltip, LIST_ROW_REMS, caption, column, compact_icon_button, icon_button,
        panel_surface, row, scrollable_list,
        search::{ListSearch, SearchEvent},
        track_list_row,
    },
    format_time,
    list_interaction::{ContentDrag, ContentMenuRequest, TrackSource},
    paged,
};

fn card_icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    hint: impl Into<SharedString>,
    cx: &mut Context<AlbumBrowser>,
    action: impl Fn(&mut AlbumBrowser, &mut Window, &mut Context<AlbumBrowser>) + 'static,
) -> Stateful<Div> {
    let theme = crate::gui::theme::palette(cx);
    let hint = hint.into();
    div()
        .id(id)
        .w(rems(1.4))
        .h(rems(2.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .font_family(super::fonts::SYMBOLS_FAMILY)
        .text_size(rems(1.))
        .text_color(rgb(theme.muted))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(theme.text)))
        .child(icon)
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.stop_propagation();
        }))
        .tooltip(move |_, cx| {
            cx.new(|_| ButtonTooltip { text: hint.clone() })
                .into()
        })
}

const ALBUM_PAGE: usize = 24;

#[derive(Clone, Copy, PartialEq)]
struct AlbumGridLayout {
    columns: usize,
    artwork_size: Pixels,
    metadata_lift: Pixels,
    row_height: Pixels,
}

impl AlbumGridLayout {
    fn measured(width: Pixels, rem: Pixels) -> Self {
        let columns = ((width / rem) / 12.).floor().max(1.) as usize;
        let card_width = (width - rem * 0.5 * columns.saturating_sub(1) as f32) / columns as f32;
        let artwork_size = (card_width - rem).max(px(0.));
        let metadata_lift = rem * 0.75 + artwork_size * (1.0 / std::f32::consts::E - 0.25);
        Self {
            columns,
            artwork_size,
            metadata_lift,
            // Cover, three metadata lines, card padding and the grid-row gap;
            // metadata follows the fade's upper edge as its coverage increases.
            row_height: artwork_size + rem * 5.125 - metadata_lift,
        }
    }
}

struct NegativeCover {
    identity: ArtworkIdentity,
    path: PathBuf,
}

impl NegativeCover {
    fn matches(&self, track: &Track) -> bool {
        self.identity.matches(track) && self.path == track.path
    }
}

struct AlbumCover {
    view: ArtworkView,
    // Only explicit negative results outlive shared-cache eviction, and only
    // for this visibly demanded source. Positive results live in the view.
    negative: Option<NegativeCover>,
}

impl AlbumCover {
    fn new(images: ArtworkImages) -> Self {
        Self {
            view: ArtworkView::new(images),
            negative: None,
        }
    }

    fn needs_load(&self, track: &Track, manager: &ArtworkManager) -> bool {
        !self
            .negative
            .as_ref()
            .is_some_and(|negative| negative.matches(track))
            && !manager.is_loaded(track)
    }

    fn complete(
        &mut self,
        source: &Track,
        current: Option<&Track>,
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
        let loaded = loaded.filter(|artwork| !artwork.is_placeholder());
        self.negative = loaded.is_none().then(|| NegativeCover {
            identity: ArtworkIdentity::for_track(source),
            path: source.path.clone(),
        });
        self.view.replace(loaded.cloned(), cx)
    }
}

/// Sparse absolute rows and viewport-owned covers per panel. Visibility cancels
/// submissions and cover demand, not unchanged album or track data.
pub(super) struct AlbumBrowser {
    handle: CoreHandle,
    i18n: Arc<crate::i18n::Localizer>,
    localized_text: BTreeMap<&'static str, SharedString>,
    manager: Arc<ArtworkManager>,
    images: ArtworkImages,
    default_album: Entity<Artwork>,
    visible: bool,
    revisions: QueryRevisions,
    albums_projection: QueryProjection,
    tracks_projection: QueryProjection,
    tracks_key: Option<AlbumKey>,
    page_size: usize,
    worker_pending: bool,
    albums_total: Option<(crate::model::QueryStamp, usize)>,
    tracks_total: Option<(crate::model::QueryStamp, usize)>,
    loading_text: super::loading::LoadingText<()>,
    error: Option<String>,
    selected: Option<AlbumKey>,
    albums: paged::PagedRows<AlbumSummary>,
    tracks: paged::PagedRows<LibraryRow>,
    covers: BTreeMap<usize, AlbumCover>,
    loader: ArtworkLoader,
    demand: Range<usize>,
    grid_layout: AlbumGridLayout,
    grid_scroll: UniformListScrollHandle,
    track_scroll: UniformListScrollHandle,
    grid_anchor: Option<AlbumKey>,
    grid_anchor_index: usize,
    grid_reset: bool,
    album_search: Entity<ListSearch>,
    tracks_search: Entity<ListSearch>,
    query: Option<String>,
    tracks_query: Option<String>,
    sort: AlbumSort,
    current_track: Option<i64>,
    playback_status: PlaybackStatus,
    _search_subscriptions: Vec<Subscription>,
}

impl EventEmitter<ContentMenuRequest> for AlbumBrowser {}

impl AlbumBrowser {
    pub(super) fn new(
        i18n: Arc<crate::i18n::Localizer>,
        handle: CoreHandle,
        manager: Arc<ArtworkManager>,
        default_album: Entity<Artwork>,
        images: ArtworkImages,
        cx: &mut Context<Self>,
    ) -> Self {
        let localized_text = [
            "gl-album-no-tracks",
            "gl-album-tracks-no-matches",
            "gl-album-sort-artist",
            "gl-album-sort-title",
            "gl-albums",
            "gl-albums-empty",
            "gl-albums-no-matches",
            "gl-back-albums",
            "gl-current",
            "gl-enqueue-album",
            "gl-enqueue-track",
            "gl-missing",
            "gl-paused",
            "gl-play-album",
            "gl-play-track",
            "gl-playing",
            "gl-retry-loading",
        ]
        .into_iter()
        .map(|id| (id, SharedString::from(i18n.text(id).to_owned())))
        .collect();
        let album_search = cx.new(|cx| ListSearch::new(i18n.clone(), cx));
        let tracks_search = cx.new(|cx| ListSearch::new(i18n.clone(), cx));
        let album_subscription = cx.subscribe(&album_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            if *query != this.query {
                this.query = query.clone();
                this.reset_grid();
                this.load(cx);
            }
        });
        let tracks_subscription = cx.subscribe(&tracks_search, |this, _, event, cx| {
            let SearchEvent::Changed(query) = event;
            if *query != this.tracks_query {
                this.tracks_query = query.clone();
                this.tracks.clear();
                this.track_scroll
                    .scroll_to_item_strict(0, ScrollStrategy::Top);
                this.load(cx);
            }
        });
        Self {
            i18n,
            localized_text,
            handle,
            manager,
            images,
            default_album,
            visible: false,
            revisions: QueryRevisions::default(),
            albums_projection: QueryProjection::default(),
            tracks_projection: QueryProjection::default(),
            tracks_key: None,
            page_size: 1,
            worker_pending: false,
            albums_total: None,
            tracks_total: None,
            loading_text: super::loading::LoadingText::default(),
            error: None,
            selected: None,
            albums: paged::PagedRows::default(),
            tracks: paged::PagedRows::default(),
            covers: BTreeMap::new(),
            loader: ArtworkLoader::default(),
            demand: 0..0,
            grid_layout: AlbumGridLayout::measured(px(192.), px(16.)),
            grid_scroll: UniformListScrollHandle::new(),
            track_scroll: UniformListScrollHandle::new(),
            grid_anchor: None,
            grid_anchor_index: 0,
            grid_reset: false,
            album_search,
            tracks_search,
            query: None,
            tracks_query: None,
            sort: AlbumSort::default(),
            current_track: None,
            playback_status: PlaybackStatus::Stopped,
            _search_subscriptions: vec![album_subscription, tracks_subscription],
        }
    }

    fn tr(&self, id: &str) -> SharedString {
        self.localized_text
            .get(id)
            .expect("cached album text")
            .clone()
    }

    fn active_search(&self) -> &Entity<ListSearch> {
        if self.selected.is_some() {
            &self.tracks_search
        } else {
            &self.album_search
        }
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.visible && self.active_search().read(cx).input_focused(window, cx)
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
        let visibility_changed = self.visible != visible;
        let demand_changed = self.page_size != page_size.max(1);
        self.revisions = revisions;
        let data_changed = self.sync_data(cx);
        self.page_size = page_size.max(1);
        if visibility_changed && !visible {
            self.cancel(cx);
        }
        self.visible = visible;
        if visible && (visibility_changed || data_changed || demand_changed) {
            if self.selected.is_none() {
                self.restore_grid_anchor();
            }
            self.request_next(cx);
        }
        if visibility_changed || data_changed || demand_changed {
            cx.notify();
        }
    }

    fn clear_covers(&mut self, cx: &mut App) {
        self.demand = 0..0;
        let _ = self.loader.prepare(None);
        for cover in std::mem::take(&mut self.covers).into_values() {
            cover.view.replace(None, cx);
        }
    }

    fn albums_query(&self) -> Query {
        Query::Albums {
            query: self.query.clone(),
            sort: self.sort,
            offset: 0,
            limit: ALBUM_PAGE,
        }
    }

    fn sync_data(&mut self, cx: &mut App) -> bool {
        let albums_query = self.albums_query();
        if self.albums_projection.stamp() != Some(albums_query.stamp(self.revisions)) {
            self.capture_grid_anchor();
        }
        let albums_changed = sync_pages(
            &mut self.albums_projection,
            &mut self.albums,
            &albums_query,
            self.revisions,
        );
        let tracks_changed = self.tracks_key.as_ref().is_some_and(|key| {
            sync_pages(
                &mut self.tracks_projection,
                &mut self.tracks,
                &Query::AlbumTracks {
                    key: key.clone(),
                    query: self.tracks_query.clone(),
                    offset: 0,
                    limit: self.page_size,
                },
                self.revisions,
            )
        });
        if albums_changed {
            self.album_search
                .update(cx, |search, cx| search.clear_counts(cx));
            self.clear_covers(cx);
        }
        if tracks_changed {
            self.tracks_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
        if (self.selected.is_none() && albums_changed)
            || (self.selected.is_some() && tracks_changed)
        {
            self.loading_text.clear();
        }
        albums_changed || tracks_changed
    }

    fn active_projection(&self) -> &QueryProjection {
        if self.selected.is_some() {
            &self.tracks_projection
        } else {
            &self.albums_projection
        }
    }

    fn cancel(&mut self, cx: &mut App) {
        self.capture_grid_anchor();
        self.loading_text.clear();
        cancel_pages(&mut self.albums_projection, &mut self.albums);
        cancel_pages(&mut self.tracks_projection, &mut self.tracks);
        // The detached worker still owns the physical gate until it completes.
        self.clear_covers(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.cancel(cx);
        self.sync_data(cx);
        if !self.visible {
            return;
        }
        if self.selected.is_none() {
            self.restore_grid_anchor();
        }
        self.error = None;
        self.request_next(cx);
    }

    fn request_next(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.worker_pending {
            return;
        }
        let query = match &self.selected {
            Some(key) => {
                let Some(offset) = self.tracks.demand(self.tracks.visible(), self.page_size) else {
                    return;
                };
                self.tracks.begin(offset, self.page_size);
                Query::AlbumTracks {
                    key: key.clone(),
                    query: self.tracks_query.clone(),
                    offset,
                    limit: self.page_size,
                }
            }
            None => {
                let Some(offset) = self.albums.demand(self.albums.visible(), ALBUM_PAGE) else {
                    return;
                };
                self.albums.begin(offset, ALBUM_PAGE);
                Query::Albums {
                    query: self.query.clone(),
                    sort: self.sort,
                    offset,
                    limit: ALBUM_PAGE,
                }
            }
        };
        let count_query = match &query {
            Query::Albums {
                query: Some(_),
                sort,
                ..
            } => Some(Query::Albums {
                query: None,
                sort: *sort,
                offset: 0,
                limit: 0,
            }),
            Query::AlbumTracks {
                key,
                query: Some(_),
                ..
            } => Some(Query::AlbumTracks {
                key: key.clone(),
                query: None,
                offset: 0,
                limit: 0,
            }),
            _ => None,
        }
        .filter(|count| {
            let total = if self.selected.is_some() {
                self.tracks_total
            } else {
                self.albums_total
            };
            total.is_none_or(|(stamp, _)| stamp != count.stamp(self.revisions))
        });
        self.worker_pending = true;
        let generation = self.active_projection().generation();
        let ticket = if self.selected.is_some() {
            self.tracks_projection.ticket()
        } else {
            self.albums_projection.ticket()
        };
        self.loading_text.begin((), generation, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let worker_query = query.clone();
            let (response, count_response) = cx
                .background_executor()
                .spawn(async move {
                    let response = handle.query_if_current(worker_query, ticket.clone());
                    let count = count_query.map(|query| {
                        let response = handle.query_if_current(query.clone(), ticket);
                        (query, response)
                    });
                    (response, count)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.loading_text.finish(&(), generation);
                this.loading_text
                    .schedule(cx, |this| &mut this.loading_text);
                // Completion, not logical cancellation, releases the physical gate.
                this.worker_pending = false;
                this.revisions = this.handle.gui_snapshot().query_revisions(None);
                this.sync_data(cx);
                if !this.visible
                    || !this
                        .active_projection()
                        .accepts(&query, generation, response.revisions)
                {
                    this.request_next(cx);
                    cx.notify();
                    return;
                }
                if let Some((count_query, count_response)) = count_response
                    && count_query.stamp(count_response.revisions)
                        == count_query.stamp(this.revisions)
                {
                    let total = match count_response.result {
                        Ok(ViewResponse::Albums(page)) => Some(page.total),
                        Ok(ViewResponse::AlbumTracks(page)) => Some(page.total),
                        _ => None,
                    };
                    if let Some(total) = total {
                        let total = Some((count_query.stamp(count_response.revisions), total));
                        if this.selected.is_some() {
                            this.tracks_total = total;
                        } else {
                            this.albums_total = total;
                        }
                    }
                }
                match response.result {
                    Ok(ViewResponse::Albums(page)) => {
                        let stamp = Query::Albums {
                            query: None,
                            sort: this.sort,
                            offset: 0,
                            limit: 0,
                        }
                        .stamp(response.revisions);
                        if this.query.is_none() {
                            this.albums_total = Some((stamp, page.total));
                        }
                        if let Some((_, total)) = this.albums_total.filter(|(s, _)| *s == stamp) {
                            this.album_search
                                .update(cx, |search, cx| search.set_counts(page.total, total, cx));
                        }
                        this.albums.accept(page);
                        this.prepare_cover(cx);
                    }
                    Ok(ViewResponse::AlbumTracks(page)) => {
                        let Query::AlbumTracks { key, .. } = &query else {
                            unreachable!()
                        };
                        let stamp = Query::AlbumTracks {
                            key: key.clone(),
                            query: None,
                            offset: 0,
                            limit: 0,
                        }
                        .stamp(response.revisions);
                        if this.tracks_query.is_none() {
                            this.tracks_total = Some((stamp, page.total));
                        }
                        if let Some((_, total)) = this.tracks_total.filter(|(s, _)| *s == stamp) {
                            this.tracks_search
                                .update(cx, |search, cx| search.set_counts(page.total, total, cx));
                        }
                        this.tracks.accept(page);
                    }
                    Err(error) => this.fail_page(error.to_string()),
                    _ => this.fail_page(this.i18n.text("gl-album-response").to_owned()),
                }
                this.request_next(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn fail_page(&mut self, message: String) {
        if self.selected.is_some() {
            self.tracks.fail(message);
        } else {
            self.albums.fail(message);
        }
    }

    pub(super) fn retry(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.tracks.retry();
        } else {
            self.albums.retry();
        }
        self.request_next(cx);
        cx.notify();
    }

    fn loading_row(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let theme = crate::gui::theme::palette(cx);
        let error = if self.selected.is_some() {
            self.tracks.error(index)
        } else {
            self.albums.error(index)
        };
        column()
            .overflow_hidden()
            .child(
                caption(
                    theme,
                    error.unwrap_or(self.i18n.text("gl-loading")).to_owned(),
                )
                .when(!self.loading_text.show(&(), error.is_some()), |text| {
                    text.invisible()
                }),
            )
            .when(error.is_some(), |view| {
                view.child(icon_button(
                    ("album-retry", index),
                    "󰑐",
                    self.tr("gl-retry-loading"),
                    cx,
                    |this, _, cx| this.retry(cx),
                ))
            })
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn open(&mut self, key: AlbumKey, cx: &mut Context<Self>) {
        self.capture_grid_anchor();
        if self.tracks_key.as_ref() != Some(&key) {
            self.tracks_key = Some(key.clone());
            self.tracks_total = None;
            self.tracks_query = None;
            self.tracks_search.update(cx, |search, cx| search.reset(cx));
            self.tracks.clear();
            self.track_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.selected = Some(key);
        self.load(cx);
    }

    fn reset_grid(&mut self) {
        self.albums.clear();
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
        if self.grid_reset || self.selected.is_some() || self.albums.is_empty() {
            return;
        }
        let state = self.grid_scroll.0.borrow();
        let grid_row = state.deferred_scroll_to_item.as_ref().map_or_else(
            || {
                // UniformList virtualizes its children; ScrollHandle's child index
                // is not the logical row. Use measured content height instead.
                state.last_item_size.map_or(0, |size| {
                    let height = size.contents.height
                        / self.albums.len().div_ceil(self.grid_layout.columns) as f32;
                    (-state.base_handle.offset().y / height).floor().max(0.) as usize
                })
            },
            |target| target.item_index,
        );
        let previous = self
            .grid_anchor
            .as_ref()
            .and_then(|key| self.albums.position(|album| &album.key == key))
            .or(Some(self.grid_anchor_index));
        let index = anchored_index(
            previous,
            grid_row,
            self.grid_layout.columns,
            self.albums.len(),
        );
        self.grid_anchor = self.albums.get(index).map(|album| album.key.clone());
        self.grid_anchor_index = index;
    }

    fn restore_grid_anchor(&mut self) {
        let index = self
            .grid_anchor
            .as_ref()
            .and_then(|key| self.albums.position(|album| &album.key == key))
            .unwrap_or(self.grid_anchor_index)
            .min(self.albums.len().saturating_sub(1));
        self.grid_scroll
            .scroll_to_item_strict(index / self.grid_layout.columns, ScrollStrategy::Top);
        self.grid_anchor = self.albums.get(index).map(|album| album.key.clone());
        self.grid_anchor_index = index;
        self.grid_reset = false;
    }

    fn cover_demand(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if !self.visible || self.selected.is_some() {
            return;
        }
        let end = range
            .end
            .saturating_mul(self.grid_layout.columns)
            .min(self.albums.len());
        let start = range
            .start
            .saturating_mul(self.grid_layout.columns)
            .min(end);
        self.demand = start..end;
        // Updating demand while a request is pending still records the latest viewport.
        let _ = self.albums.demand(self.demand.clone(), ALBUM_PAGE);
        self.prepare_cover(cx);
        self.request_next(cx);
    }

    fn prepare_cover(&mut self, cx: &mut Context<Self>) {
        if !self.visible || self.selected.is_some() {
            return;
        }
        for (_, cover) in self.covers.extract_if(.., |index, _| {
            !self.demand.contains(index) || self.albums.get(*index).is_none()
        }) {
            cover.view.replace(None, cx);
        }
        let mut changed = false;
        for index in self.demand.clone() {
            let Some(album) = self.albums.get(index) else {
                continue;
            };
            let cover = self
                .covers
                .entry(index)
                .or_insert_with(|| AlbumCover::new(self.images.clone()));
            if cover
                .negative
                .as_ref()
                .is_some_and(|negative| !negative.matches(&album.representative))
            {
                cover.negative = None;
            }
            changed |= cover.view.replace(
                self.manager
                    .cached(&album.representative)
                    .filter(|artwork| !artwork.is_placeholder()),
                cx,
            );
        }
        let next = self.demand.clone().find_map(|index| {
            let track = &self.albums.get(index)?.representative;
            self.covers
                .get(&index)?
                .needs_load(track, &self.manager)
                .then_some(track)
        });
        if let Some(track) = self.loader.prepare(next) {
            let identity = ArtworkIdentity::for_track(&track);
            let manager = Arc::clone(&self.manager);
            cx.spawn(async move |this, cx| {
                let (track, loaded) = cx
                    .background_executor()
                    .spawn(async move {
                        let loaded = manager.load(&track);
                        (track, loaded)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // The worker result remains leased through direct delivery.
                    // Hidden/obsolete demand can warm the shared cache only.
                    let _ = this.loader.finish(&identity, None);
                    if !this.visible || this.selected.is_some() {
                        return;
                    }
                    let mut changed = false;
                    for index in this.demand.clone() {
                        let current = this.albums.get(index).map(|album| &album.representative);
                        if let Some(cover) = this.covers.get_mut(&index) {
                            changed |= cover.complete(&track, current, loaded.as_ref(), cx);
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                    this.prepare_cover(cx);
                });
            })
            .detach();
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn retire_all_images(&mut self, cx: &mut App) {
        self.clear_covers(cx);
    }

    pub(super) fn release_all_images(&mut self, window: &mut Window, cx: &mut App) {
        for cover in self.covers.values() {
            cover.view.release_all_images(window, cx);
        }
    }

    fn card(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = crate::gui::theme::palette(cx);
        let Some(album) = self.albums.get(index) else {
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
        let image = self
            .covers
            .get(&index)
            .and_then(|cover| cover.view.image(cx));
        let directory = key.directory.to_string_lossy().into_owned();
        let title = key.title.clone();
        let drag_label: SharedString = title.clone().into();
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
            .right(rems(0.5))
            .bottom(rems(0.5))
            .gap_0()
            .invisible()
            .group_hover("album-card", |style| style.visible())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(card_icon_button(
                ("play-album", index),
                "",
                self.tr("gl-play-album"),
                cx,
                move |this, _, cx| {
                    this.send(
                        Command::PlayAlbum {
                            key: play_key.clone(),
                        },
                        cx,
                    )
                },
            ))
            .child(card_icon_button(
                ("enqueue-album", index),
                "󰐕",
                self.tr("gl-enqueue-album"),
                cx,
                move |this, _, cx| {
                    this.send(
                        Command::EnqueueAlbum {
                            key: enqueue_key.clone(),
                        },
                        cx,
                    )
                },
            ));
        column()
            .id(("album-card", index))
            .group("album-card")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_2()
            .gap_0()
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.open(key.clone(), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(ContentMenuRequest {
                        source: TrackSource::Album(menu_key.clone()),
                        position: event.position,
                        play: Some(Command::PlayAlbum {
                            key: menu_key.clone(),
                        }),
                    });
                }),
            )
            .on_drag(
                ContentDrag::new(TrackSource::Album(drag_key)),
                move |_, _, window, cx| {
                    super::components::begin_list_drag(window, cx);
                    cx.new(|_| super::components::ListDragPreview {
                        label: drag_label.clone(),
                    })
                },
            )
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
                        Some(image) => {
                            view.child(image.absolute().size_full().object_fit(ObjectFit::Contain))
                        }
                        None => view.child(self.default_album.clone()),
                    })
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .w_full()
                            .h(self.grid_layout.artwork_size / std::f32::consts::E)
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
                    .h(rems(1.25))
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
                .h(rems(1.125))
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
                .h(rems(1.25))
                .line_height(rems(1.25))
                .truncate()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0(),
            )
            .child(actions)
            .into_any_element()
    }
}

fn sync_pages<T>(
    projection: &mut QueryProjection,
    pages: &mut paged::PagedRows<T>,
    query: &Query,
    revisions: QueryRevisions,
) -> bool {
    if !projection.sync(query, revisions) {
        return false;
    }
    // Unknown total bootstraps a new request even after an accepted empty result.
    pages.clear();
    true
}

fn cancel_pages<T>(projection: &mut QueryProjection, pages: &mut paged::PagedRows<T>) {
    projection.cancel();
    pages.cancel_pending();
}

// Reflow can place earlier cards before the anchor in its new row. Keep the
// identity until the user actually scrolls to a different row.
fn anchored_index(previous: Option<usize>, row: usize, columns: usize, count: usize) -> usize {
    previous
        .filter(|index| *index / columns == row)
        .unwrap_or_else(|| row.saturating_mul(columns))
        .min(count.saturating_sub(1))
}

fn track_marker(current: Option<i64>, status: PlaybackStatus, id: i64) -> Option<&'static str> {
    (current == Some(id)).then_some(match status {
        PlaybackStatus::Playing => "gl-playing",
        PlaybackStatus::Paused => "gl-paused",
        PlaybackStatus::Stopped => "gl-current",
    })
}

impl Render for AlbumBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::gui::theme::palette(cx);
        let total = if self.selected.is_some() {
            self.tracks_total
        } else {
            self.albums_total
        }
        .filter(|(stamp, _)| Some(*stamp) == self.active_projection().stamp())
        .map(|(_, total)| total);
        let mut header = row().items_end().flex_shrink_0().min_w_0().px_2();
        if let Some(key) = &self.selected {
            let play_key = key.clone();
            let enqueue_key = key.clone();
            let menu_key = key.clone();
            header = header
                .child(icon_button(
                    "albums-back",
                    "",
                    self.tr("gl-back-albums"),
                    cx,
                    |this, _, cx| {
                        this.selected = None;
                        this.load(cx);
                    },
                ))
                .child(
                    div()
                        .id("album-selected-title")
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(ContentMenuRequest {
                                    source: TrackSource::Album(menu_key.clone()),
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
                )
                .child(self.tracks_search.clone())
                .child(icon_button(
                    "album-play-all",
                    "",
                    self.tr("gl-play-album"),
                    cx,
                    move |this, _, cx| {
                        this.send(
                            Command::PlayAlbum {
                                key: play_key.clone(),
                            },
                            cx,
                        )
                    },
                ))
                .child(icon_button(
                    "album-add-all",
                    "󰐕",
                    self.tr("gl-enqueue-album"),
                    cx,
                    move |this, _, cx| {
                        this.send(
                            Command::EnqueueAlbum {
                                key: enqueue_key.clone(),
                            },
                            cx,
                        )
                    },
                ));
        } else {
            header = header
                .child(
                    crate::gui::components::panel_header(
                        theme,
                        self.tr("gl-albums"),
                        total.map_or_else(
                            || "—".to_owned(),
                            |count| {
                                self.i18n
                                    .format("gl-entry-count", &[("count", count.into())])
                            },
                        ),
                    )
                    .flex_1()
                    .min_w_0(),
                )
                .child(self.album_search.clone())
                .child(icon_button(
                    "albums-sort",
                    match self.sort {
                        AlbumSort::Title => "󰉿",
                        AlbumSort::Artist => "󰀄",
                    },
                    match self.sort {
                        AlbumSort::Title => self.tr("gl-album-sort-title"),
                        AlbumSort::Artist => self.tr("gl-album-sort-artist"),
                    },
                    cx,
                    |this, _, cx| {
                        this.sort = match this.sort {
                            AlbumSort::Title => AlbumSort::Artist,
                            AlbumSort::Artist => AlbumSort::Title,
                        };
                        this.reset_grid();
                        this.load(cx);
                    },
                ));
        }
        let generation = self.active_projection().generation();
        let track_demand_owner = cx.weak_entity();
        let content = if self.selected.is_some() {
            if self.tracks.is_empty() {
                column()
                    .px_2()
                    .child(caption(
                        theme,
                        self.tr(if self.tracks_query.is_some() && total != Some(0) {
                            "gl-album-tracks-no-matches"
                        } else {
                            "gl-album-no-tracks"
                        }),
                    ))
                    .into_any_element()
            } else {
                scrollable_list(
                    "album-tracks-scrollbar",
                    uniform_list(
                        "album-tracks",
                        self.tracks.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let Some(track) = this.tracks.get(index) else {
                                        return this
                                            .loading_row(index, cx)
                                            .h(rems(LIST_ROW_REMS))
                                            .w_full()
                                            .p_2()
                                            .into_any_element();
                                    };
                                    let id = track.id;
                                    let drag_label: SharedString = track.title.clone().into();
                                    let marker =
                                        track_marker(this.current_track, this.playback_status, id);
                                    track_list_row(
                                        theme,
                                        ("album-track", id as u64),
                                        ("album-track-text", id as u64),
                                        (
                                            track.title.clone(),
                                            if track.artist.trim().is_empty() {
                                                "-".to_owned()
                                            } else {
                                                track.artist.clone()
                                            },
                                        ),
                                        index,
                                        marker.is_some()
                                            && this.playback_status == PlaybackStatus::Playing,
                                        false,
                                    )
                                    .group("album-track-row")
                                    .line_height(rems(1.125))
                                    .relative()
                                    .gap_0()
                                    .on_click(cx.listener(
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
                                            super::components::begin_list_drag(window, cx);
                                            cx.new(|_| super::components::ListDragPreview {
                                                label: drag_label.clone(),
                                            })
                                        },
                                    )
                                    .when(track.missing, |row| {
                                        row.child(caption(theme, this.tr("gl-missing")))
                                    })
                                    .when_some(marker, |row, marker| {
                                        row.child(caption(theme, this.tr(marker)))
                                    })
                                    .child(
                                        caption(
                                            theme,
                                            track.duration.map_or_else(|| "—".into(), format_time),
                                        )
                                        .font_features(super::fonts::tabular_numbers())
                                        .w(rems(4.))
                                        .flex_shrink_0()
                                        .text_right(),
                                    )
                                    .child(
                                        super::components::hover_actions(
                                            theme,
                                            "album-track-row",
                                            false,
                                        )
                                        .id(("album-track-actions", id as u64))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .on_click(|_, _, cx| cx.stop_propagation())
                                        .child(compact_icon_button(
                                            theme,
                                            icon_button(
                                                ("album-track-play", id as u64),
                                                "",
                                                this.tr("gl-play-track"),
                                                cx,
                                                move |this, _, cx| {
                                                    this.send(Command::Play { track_id: id }, cx)
                                                },
                                            ),
                                        ))
                                        .child(
                                            compact_icon_button(
                                                theme,
                                                icon_button(
                                                    ("album-track-add", id as u64),
                                                    "󰐕",
                                                    this.tr("gl-enqueue-track"),
                                                    cx,
                                                    move |this, _, cx| {
                                                        this.send(
                                                            Command::Enqueue {
                                                                track_ids: vec![id],
                                                            },
                                                            cx,
                                                        )
                                                    },
                                                ),
                                            ),
                                        ),
                                    )
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.track_scroll)
                    .with_decoration(paged::viewport_demand(move |range, _, cx| {
                        let owner = track_demand_owner.clone();
                        cx.defer(move |cx| {
                            let _ = owner.update(cx, |this, cx| {
                                if generation == this.active_projection().generation()
                                    && this.selected.is_some()
                                    && this.visible
                                {
                                    let _ = this.tracks.demand(range, this.page_size);
                                    this.request_next(cx);
                                }
                            });
                        });
                    }))
                    .size_full(),
                    &self.track_scroll,
                )
                .into_any_element()
            }
        } else if self.albums.is_empty() {
            column()
                .px_2()
                .child(caption(
                    theme,
                    if self.query.is_some() && total != Some(0) {
                        self.tr("gl-albums-no-matches")
                    } else {
                        self.tr("gl-albums-empty")
                    },
                ))
                .into_any_element()
        } else {
            let layout = self.grid_layout;
            let columns = layout.columns;
            let generation = self.active_projection().generation();
            let demand_owner = cx.weak_entity();
            let measure_owner = cx.weak_entity();
            scrollable_list(
                "album-grid-scrollbar",
                div()
                    .relative()
                    .size_full()
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let measured =
                                    AlbumGridLayout::measured(bounds.size.width, window.rem_size());
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
                    .child(
                        uniform_list(
                            "album-grid",
                            self.albums.len().div_ceil(columns),
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                range
                                    .map(|grid_row| {
                                        let start = grid_row * columns;
                                        let end = (start + columns).min(this.albums.len());
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
                        .track_scroll(&self.grid_scroll)
                        .with_decoration(paged::viewport_demand(move |range, _, cx| {
                            let owner = demand_owner.clone();
                            cx.defer(move |cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    if generation == this.active_projection().generation()
                                        && layout == this.grid_layout
                                    {
                                        this.cover_demand(range, cx);
                                    }
                                });
                            });
                        }))
                        .size_full(),
                    ),
                &self.grid_scroll,
            )
            .into_any_element()
        };
        panel_surface("albums-browser")
            .p_2()
            .child(header)
            .when_some(self.error.clone(), |view, error| {
                view.child(caption(theme, error).px_2())
            })
            .child(div().flex_1().min_h_0().min_w_0().child(content))
    }
}

#[cfg(test)]
mod tests {
    use super::{AlbumCover, anchored_index, cancel_pages, sync_pages};
    use crate::{
        artwork::{
            ArtworkManager,
            tests::{tagged_wav, track},
        },
        gui::{artwork::ArtworkImages, paged},
        model::{AlbumKey, AlbumSort, Page, Query},
        projection::QueryProjection,
        response::QueryRevisions,
    };

    #[gpui::test]
    fn positive_cover_completion_survives_shared_cache_pressure(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.wav");
        tagged_wav(&path);
        let manager = ArtworkManager::new(directory.path());
        let mut source = track(path);
        let mut held = Vec::new();
        for id in 1..=64 {
            source.id = id;
            held.push(manager.load(&source).unwrap());
        }
        source.id = 65;
        // The worker's result must stay leased while the GUI callback is queued.
        let completed = manager.load(&source).unwrap();
        assert!(!completed.is_placeholder());
        let mut pressure = source.clone();
        pressure.id = 66;
        drop(manager.load(&pressure));

        let mut cover = AlbumCover::new(ArtworkImages::default());
        cx.update(|cx| {
            assert!(cover.complete(&source, Some(&source), Some(&completed), cx));
            assert!(cover.view.image(cx).is_some());
            assert!(cover.negative.is_none());
        });
        drop(completed);
        drop(held);
        for id in 67..140 {
            pressure.id = id;
            drop(manager.load(&pressure));
        }
        assert!(manager.is_loaded(&source));
        assert!(!cover.needs_load(&source, &manager));
        cx.update(|cx| assert!(cover.view.image(cx).is_some()));
    }

    #[gpui::test]
    fn completed_negative_covers_beyond_shared_capacity_converge(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let sources: Vec<_> = (1..=96)
            .map(|id| {
                let mut source = track(directory.path().join(format!("missing-{id}.wav")));
                source.id = id;
                source
            })
            .collect();
        let mut covers: Vec<_> = sources
            .iter()
            .map(|_| AlbumCover::new(ArtworkImages::default()))
            .collect();
        for (source, cover) in sources.iter().zip(&mut covers) {
            assert!(cover.needs_load(source, &manager));
            let completed = manager.load(source);
            cx.update(|cx| {
                cover.complete(source, Some(source), completed.as_ref(), cx);
                assert!(cover.view.image(cx).is_none());
                assert!(cover.negative.is_some());
            });
        }
        assert!(!manager.is_loaded(&sources[0]));
        for (source, cover) in sources.iter().zip(&covers) {
            assert!(!cover.needs_load(source, &manager));
        }
    }

    #[gpui::test]
    fn obsolete_or_relocated_negative_completion_cannot_claim_current_demand(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let manager = ArtworkManager::new(directory.path());
        let source = track(directory.path().join("missing.wav"));
        let negative = manager.load(&source);
        let mut current = source.clone();
        current.path = directory.path().join("available.wav");
        tagged_wav(&current.path);
        let mut cover = AlbumCover::new(ArtworkImages::default());
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
        assert!(cover.needs_load(&current, &manager));
        let recovered = manager.load(&current).unwrap();
        cx.update(|cx| {
            assert!(cover.complete(&current, Some(&current), Some(&recovered), cx));
            assert!(cover.view.image(cx).is_some());
            assert!(cover.negative.is_none());
        });
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

    fn queries() -> [Query; 2] {
        [
            Query::Albums {
                query: None,
                sort: AlbumSort::default(),
                offset: 0,
                limit: 2,
            },
            Query::AlbumTracks {
                key: AlbumKey {
                    directory: "/music".into(),
                    title: "Album".into(),
                },
                query: None,
                offset: 0,
                limit: 2,
            },
        ]
    }

    #[test]
    fn album_track_search_replaces_pages_and_rejects_previous_completion() {
        let mut projection = QueryProjection::default();
        let mut pages = paged::PagedRows::default();
        let revisions = QueryRevisions::default();
        let mut query = queries().into_iter().nth(1).unwrap();
        sync_pages(&mut projection, &mut pages, &query, revisions);
        pages.begin(0, 2);
        pages.accept(Page {
            total: 2,
            rows: vec![41, 42],
        });
        let previous = query.clone();
        let generation = projection.generation();
        let Query::AlbumTracks { query: search, .. } = &mut query else {
            unreachable!()
        };
        *search = Some("needle".into());
        assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
        assert!(pages.get(0).is_none());
        assert!(!projection.accepts(&previous, generation, revisions));
        pages.begin(0, 2);
        pages.accept(Page {
            total: 1,
            rows: vec![42],
        });
        assert_eq!(pages.get(0), Some(&42));
        let Query::AlbumTracks { query: search, .. } = &mut query else {
            unreachable!()
        };
        *search = None;
        assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
        assert!(pages.get(0).is_none());
    }

    #[test]
    fn hiding_retains_pages_and_failures_but_cancels_pending_submissions() {
        for query in queries() {
            let mut projection = QueryProjection::default();
            let mut pages = paged::PagedRows::default();
            let revisions = QueryRevisions::default();
            assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
            pages.begin(0, 2);
            pages.accept(Page {
                total: 6,
                rows: vec![10, 11],
            });
            pages.begin(2, 2);
            pages.fail("Unavailable".into());
            pages.begin(4, 2);
            let generation = projection.generation();
            cancel_pages(&mut projection, &mut pages);
            assert_eq!(pages.len(), 6);
            assert_eq!(pages.get(0), Some(&10));
            assert_eq!(pages.error(2), Some("Unavailable"));
            assert!(!projection.accepts(&query, generation, revisions));
            assert!(!sync_pages(&mut projection, &mut pages, &query, revisions));
            assert_eq!(pages.get(1), Some(&11));
            assert_eq!(pages.demand(4..6, 2), Some(4));
            assert_eq!(pages.demand(2..4, 2), Some(4));
            pages.retry();
            assert_eq!(pages.demand(2..4, 2), Some(2));
        }
    }

    #[test]
    fn hidden_data_changes_discard_pages_and_bootstrap_previous_empty_results() {
        for query in queries() {
            for revisions in [
                QueryRevisions {
                    library: 1,
                    ..Default::default()
                },
                QueryRevisions {
                    structure: 1,
                    ..Default::default()
                },
            ] {
                for total in [0, 2] {
                    let mut projection = QueryProjection::default();
                    let mut pages = paged::PagedRows::default();
                    sync_pages(
                        &mut projection,
                        &mut pages,
                        &query,
                        QueryRevisions::default(),
                    );
                    pages.begin(0, 2);
                    pages.accept(Page {
                        total,
                        rows: if total == 0 { vec![] } else { vec![1, 2] },
                    });
                    cancel_pages(&mut projection, &mut pages);
                    assert!(sync_pages(&mut projection, &mut pages, &query, revisions));
                    assert!(!pages.is_empty());
                    assert!(pages.get(0).is_none());
                    assert_eq!(pages.demand(pages.visible(), 2), Some(0));
                }
            }
        }
    }

    #[test]
    fn paging_and_unrelated_revisions_do_not_change_album_data_keys() {
        let mut projection = QueryProjection::default();
        let mut pages = paged::PagedRows::default();
        let query = queries().into_iter().next().unwrap();
        sync_pages(
            &mut projection,
            &mut pages,
            &query,
            QueryRevisions::default(),
        );
        pages.begin(0, 2);
        pages.accept(Page {
            total: 2,
            rows: vec![1, 2],
        });
        let generation = projection.generation();
        let paging = Query::Albums {
            query: None,
            sort: AlbumSort::default(),
            offset: 24,
            limit: 48,
        };
        let revisions = QueryRevisions {
            playlist: 1,
            queue: 1,
            duplicate_members: 1,
            duplicate_groups: 1,
            ..Default::default()
        };
        assert!(!sync_pages(&mut projection, &mut pages, &paging, revisions));
        assert_eq!(projection.generation(), generation);
        assert_eq!(pages.get(0), Some(&1));
        assert!(projection.accepts(&paging, generation, revisions));
        let changed = Query::Albums {
            query: Some("new search".into()),
            sort: AlbumSort::default(),
            offset: 0,
            limit: 2,
        };
        assert!(sync_pages(&mut projection, &mut pages, &changed, revisions));
        assert!(pages.get(0).is_none());
        assert_eq!(pages.demand(pages.visible(), 2), Some(0));
    }
}
