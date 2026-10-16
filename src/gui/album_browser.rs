use std::{
    collections::BTreeMap,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{prelude::*, *};

use crate::{
    artwork::{ArtworkIdentity, ArtworkManager},
    core::CoreHandle,
    model::{
        AlbumKey, AlbumSort, AlbumSummary, Command, LibraryRow, PlaybackStatus, Query,
        SEARCH_DEBOUNCE,
    },
    projection::QueryProjection,
    response::{QueryRevisions, ViewResponse},
};

use super::{
    BORDER, TEXT,
    artwork::{Artwork, ArtworkImages, ArtworkLoader, ArtworkView},
    caption, column,
    components::{LIST_ROW_REMS, quiet_icon_button, scrollable_list, track_list_row},
    format_time, icon_button,
    input::{Input, InputEvent},
    paged, panel_surface, panel_toolbar, row,
};

fn card_icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    hint: impl Into<SharedString>,
    cx: &mut Context<AlbumBrowser>,
    action: impl Fn(&mut AlbumBrowser, &mut Window, &mut Context<AlbumBrowser>) + 'static,
) -> Stateful<Div> {
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
        .text_color(rgb(super::MUTED))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(TEXT)))
        .child(icon)
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.stop_propagation();
        }))
        .tooltip(move |_, cx| {
            cx.new(|_| super::ButtonTooltip { text: hint.clone() })
                .into()
        })
}

const ALBUM_PAGE: usize = 24;

#[derive(Default)]
struct AlbumLoadState {
    // Latest search intent survives cancellation, hiding and worker completion.
    quiet_until: Option<Instant>,
    // Only a submitted Core query owns this gate, until its completion callback.
    worker_pending: bool,
}

impl AlbumLoadState {
    fn search_changed(&mut self, nonempty: bool, now: Instant) {
        self.quiet_until = nonempty.then(|| now + SEARCH_DEBOUNCE);
    }

    fn next_delay(&self, visible: bool, album_grid: bool, now: Instant) -> Option<Duration> {
        if !visible || self.worker_pending {
            return None;
        }
        Some(if album_grid {
            self.quiet_until.map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(now)
            })
        } else {
            Duration::ZERO
        })
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
    load_state: AlbumLoadState,
    search_timer: Option<Task<()>>,
    error: Option<String>,
    selected: Option<AlbumKey>,
    albums: paged::PagedRows<AlbumSummary>,
    tracks: paged::PagedRows<LibraryRow>,
    covers: BTreeMap<usize, ArtworkView>,
    loader: ArtworkLoader,
    demand: Range<usize>,
    columns: usize,
    grid_scroll: UniformListScrollHandle,
    track_scroll: UniformListScrollHandle,
    grid_anchor: Option<AlbumKey>,
    grid_anchor_index: usize,
    grid_reset: bool,
    search: Entity<Input>,
    query: Option<String>,
    sort: AlbumSort,
    current_track: Option<i64>,
    playback_status: PlaybackStatus,
    _search_subscription: Subscription,
}

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
        let search = cx.new(|cx| Input::new("", i18n.text("gl-search-albums").to_owned(), cx));
        let subscription = cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, InputEvent::Changed { .. }) {
                let text = this.search.read(cx).text().trim();
                let query = (!text.is_empty()).then(|| text.to_owned());
                if query != this.query {
                    this.query = query;
                    this.reset_grid();
                    this.load_with_delay(true, cx);
                }
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
            load_state: AlbumLoadState::default(),
            search_timer: None,
            error: None,
            selected: None,
            albums: paged::PagedRows::default(),
            tracks: paged::PagedRows::default(),
            covers: BTreeMap::new(),
            loader: ArtworkLoader::default(),
            demand: 0..0,
            columns: 1,
            grid_scroll: UniformListScrollHandle::new(),
            track_scroll: UniformListScrollHandle::new(),
            grid_anchor: None,
            grid_anchor_index: 0,
            grid_reset: false,
            search,
            query: None,
            sort: AlbumSort::default(),
            current_track: None,
            playback_status: PlaybackStatus::Stopped,
            _search_subscription: subscription,
        }
    }

    fn tr(&self, id: &str) -> SharedString {
        self.localized_text
            .get(id)
            .expect("cached album text")
            .clone()
    }

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.visible && self.selected.is_none() && self.search.read(cx).is_focused(window)
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
            cover.replace(None, cx);
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
                    offset: 0,
                    limit: self.page_size,
                },
                self.revisions,
            )
        });
        if albums_changed {
            self.clear_covers(cx);
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
        self.search_timer = None;
        cancel_pages(&mut self.albums_projection, &mut self.albums);
        cancel_pages(&mut self.tracks_projection, &mut self.tracks);
        // The detached worker still owns the physical gate until it completes.
        self.clear_covers(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.load_with_delay(false, cx);
    }

    fn load_with_delay(&mut self, debounce: bool, cx: &mut Context<Self>) {
        if debounce {
            self.load_state
                .search_changed(self.query.is_some(), Instant::now());
        }
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
        if self.search_timer.is_some() {
            return;
        }
        let Some(delay) =
            self.load_state
                .next_delay(self.visible, self.selected.is_none(), Instant::now())
        else {
            return;
        };
        if !delay.is_zero() {
            let timer = cx.background_executor().timer(delay);
            self.search_timer = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    this.search_timer = None;
                    // A wake only rechecks current demand; it owns no query or page.
                    this.request_next(cx);
                });
            }));
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
        self.load_state.worker_pending = true;
        let generation = self.active_projection().generation();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let worker_query = query.clone();
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(worker_query) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Completion, not logical cancellation, releases the physical gate.
                this.load_state.worker_pending = false;
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
                match response.result {
                    Ok(ViewResponse::Albums(page)) => {
                        this.albums.accept(page);
                        this.prepare_cover(cx);
                    }
                    Ok(ViewResponse::AlbumTracks(page)) => this.tracks.accept(page),
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

    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.tracks.retry();
        } else {
            self.albums.retry();
        }
        self.request_next(cx);
        cx.notify();
    }

    fn loading_row(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let error = if self.selected.is_some() {
            self.tracks.error(index)
        } else {
            self.albums.error(index)
        };
        column()
            .overflow_hidden()
            .child(caption(
                error.unwrap_or(self.i18n.text("gl-loading")).to_owned(),
            ))
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
                    let height =
                        size.contents.height / self.albums.len().div_ceil(self.columns) as f32;
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
        let index = anchored_index(previous, grid_row, self.columns, self.albums.len());
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
            .scroll_to_item_strict(index / self.columns, ScrollStrategy::Top);
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
            .saturating_mul(self.columns)
            .min(self.albums.len());
        let start = range.start.saturating_mul(self.columns).min(end);
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
            cover.replace(None, cx);
        }
        let mut changed = false;
        for index in self.demand.clone() {
            let Some(album) = self.albums.get(index) else {
                continue;
            };
            changed |= self
                .covers
                .entry(index)
                .or_insert_with(|| ArtworkView::new(self.images.clone()))
                .replace(
                    self.manager
                        .cached(&album.representative)
                        .filter(|artwork| !artwork.is_placeholder()),
                    cx,
                );
        }
        let next = self.demand.clone().find_map(|index| {
            let track = &self.albums.get(index)?.representative;
            (!self.manager.is_loaded(track)).then_some(track)
        });
        if let Some(track) = self.loader.prepare(next) {
            let identity = ArtworkIdentity::for_track(&track);
            let manager = Arc::clone(&self.manager);
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .spawn(async move { manager.load(&track) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // Finish the shared load even after hiding; never start the
                    // next one unless the latest viewport still demands it.
                    let _ = this.loader.finish(&identity, None);
                    if this.visible {
                        this.prepare_cover(cx);
                    }
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
            cover.release_all_images(window, cx);
        }
    }

    fn card(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
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
        let image = self.covers.get(&index).and_then(|cover| cover.image(cx));
        let directory = key.directory.to_string_lossy().into_owned();
        let title = album_title(&self.i18n, &key);
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
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    this.open(key.clone(), cx);
                }
            }))
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .overflow_hidden()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(rems(7.))
                    .flex_shrink_0()
                    .map(|view| match image {
                        Some(image) => {
                            view.child(image.absolute().size_full().object_fit(ObjectFit::Contain))
                        }
                        None => view.child(self.default_album.clone()),
                    }),
            )
            .child(
                div()
                    .id(("album-title", index))
                    .w_full()
                    .h(rems(1.5))
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .text_center()
                    .truncate()
                    .child(title.clone())
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::ButtonTooltip {
                            text: format!("{title}\n{directory}").into(),
                        })
                        .into()
                    }),
            )
            .child(
                caption(album.artist.clone())
                    .w_full()
                    .text_center()
                    .truncate()
                    .h(rems(1.25))
                    .flex_shrink_0(),
            )
            .child(
                caption(
                    self.i18n
                        .format("gl-track-count", &[("count", album.track_count.into())]),
                )
                .w_full()
                .text_center()
                .h(rems(1.25))
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

fn album_title(i18n: &crate::i18n::Localizer, key: &AlbumKey) -> String {
    if key.title.is_empty() {
        i18n.text("gl-unknown-album").into()
    } else {
        key.title.clone()
    }
}

impl Render for AlbumBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut header = panel_toolbar();
        if let Some(key) = &self.selected {
            let play_key = key.clone();
            let enqueue_key = key.clone();
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
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(TEXT))
                        .child(album_title(&self.i18n, key)),
                )
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
                .child(div().flex_1().min_w_0().child(self.search.clone()))
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
                    .child(caption(self.tr("gl-album-no-tracks")))
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
                                    let marker =
                                        track_marker(this.current_track, this.playback_status, id);
                                    track_list_row(
                                        ("album-track", id as u64),
                                        ("album-track-text", id as u64),
                                        track.title.clone(),
                                        track.artist.clone(),
                                        index,
                                        marker.is_some()
                                            && this.playback_status == PlaybackStatus::Playing,
                                        false,
                                    )
                                    .group("album-track-row")
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, _, cx| {
                                            if event.click_count() == 2 {
                                                this.send(Command::Play { track_id: id }, cx);
                                            }
                                        },
                                    ))
                                    .when(track.missing, |row| {
                                        row.child(caption(this.tr("gl-missing")))
                                    })
                                    .when_some(marker, |row, marker| {
                                        row.child(caption(this.tr(marker)))
                                    })
                                    .child(
                                        caption(
                                            track.duration.map_or_else(|| "—".into(), format_time),
                                        )
                                        .w(rems(4.))
                                        .flex_shrink_0()
                                        .text_right(),
                                    )
                                    .child(
                                        row()
                                            .id(("album-track-actions", id as u64))
                                            .gap_0()
                                            .invisible()
                                            .group_hover("album-track-row", |style| style.visible())
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_click(|_, _, cx| cx.stop_propagation())
                                            .child(
                                                quiet_icon_button(icon_button(
                                                    ("album-track-play", id as u64),
                                                    "",
                                                    this.tr("gl-play-track"),
                                                    cx,
                                                    move |this, _, cx| {
                                                        this.send(
                                                            Command::Play { track_id: id },
                                                            cx,
                                                        )
                                                    },
                                                ))
                                                .w(rems(1.4)),
                                            )
                                            .child(
                                                quiet_icon_button(icon_button(
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
                                                ))
                                                .w(rems(1.4)),
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
                .child(caption(if self.query.is_some() {
                    self.tr("gl-albums-no-matches")
                } else {
                    self.tr("gl-albums-empty")
                }))
                .into_any_element()
        } else {
            let columns = self.columns;
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
                                let columns = ((bounds.size.width / window.rem_size()) / 12.)
                                    .floor()
                                    .max(1.) as usize;
                                let owner = measure_owner.clone();
                                cx.defer(move |cx| {
                                    let _ = owner.update(cx, |this, cx| {
                                        if this.columns != columns {
                                            this.capture_grid_anchor();
                                            this.columns = columns;
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
                                            .h(rems(14.))
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
                                        && columns == this.columns
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
            .when_some(self.error.clone(), |view, error| view.child(caption(error)))
            .child(div().flex_1().min_h_0().min_w_0().child(content))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{AlbumLoadState, anchored_index, cancel_pages, sync_pages};
    use crate::{
        gui::paged,
        model::{AlbumKey, AlbumSort, Page, Query, SEARCH_DEBOUNCE},
        projection::QueryProjection,
        response::QueryRevisions,
    };

    #[test]
    fn replacing_unsubmitted_search_waits_for_latest_quiet_deadline() {
        let start = Instant::now();
        let mut load = AlbumLoadState::default();
        load.search_changed(true, start);
        assert_eq!(load.next_delay(true, true, start), Some(SEARCH_DEBOUNCE));

        let replacement = start + Duration::from_millis(100);
        load.search_changed(true, replacement);
        assert_eq!(
            load.next_delay(true, true, start + SEARCH_DEBOUNCE),
            Some(Duration::from_millis(100))
        );
        let deadline = replacement + SEARCH_DEBOUNCE;
        assert_eq!(
            load.next_delay(true, true, deadline - Duration::from_nanos(1)),
            Some(Duration::from_nanos(1))
        );
        assert_eq!(load.next_delay(true, true, deadline), Some(Duration::ZERO));
    }

    #[test]
    fn submitted_search_completion_preserves_replacement_deadline_and_immediate_paging() {
        let start = Instant::now();
        let mut load = AlbumLoadState {
            worker_pending: true,
            ..Default::default()
        };
        let replacement = start + Duration::from_millis(100);
        load.search_changed(true, replacement);
        let completed = start + SEARCH_DEBOUNCE;
        assert_eq!(load.next_delay(true, true, completed), None);

        load.worker_pending = false;
        assert_eq!(
            load.next_delay(true, true, completed),
            Some(Duration::from_millis(100))
        );
        let deadline = replacement + SEARCH_DEBOUNCE;
        assert_eq!(load.next_delay(true, true, deadline), Some(Duration::ZERO));
        load.worker_pending = true;
        // Even an elapsed quiet deadline cannot release the physical worker.
        let next_page = deadline + Duration::from_secs(1);
        assert_eq!(load.next_delay(true, true, next_page), None);
        load.worker_pending = false;
        assert_eq!(load.next_delay(true, true, next_page), Some(Duration::ZERO));
    }

    #[test]
    fn clearing_search_removes_delay_without_releasing_submitted_worker() {
        let start = Instant::now();
        let mut load = AlbumLoadState::default();
        load.search_changed(true, start);
        let cleared = start + Duration::from_millis(100);
        load.search_changed(false, cleared);
        assert_eq!(load.next_delay(true, true, cleared), Some(Duration::ZERO));

        load.worker_pending = true;
        load.search_changed(true, cleared);
        load.search_changed(false, cleared);
        assert_eq!(load.next_delay(true, true, cleared), None);
        load.worker_pending = false;
        assert_eq!(load.next_delay(true, true, cleared), Some(Duration::ZERO));
    }

    #[test]
    fn hidden_and_selected_album_transitions_retain_grid_quiet_deadline() {
        let start = Instant::now();
        let mut load = AlbumLoadState::default();
        load.search_changed(true, start);
        let reopened = start + Duration::from_millis(100);
        assert_eq!(load.next_delay(false, true, reopened), None);
        assert_eq!(load.next_delay(true, false, reopened), Some(Duration::ZERO));
        assert_eq!(
            load.next_delay(true, true, reopened),
            Some(SEARCH_DEBOUNCE - Duration::from_millis(100))
        );
        assert_eq!(
            load.next_delay(true, true, start + SEARCH_DEBOUNCE),
            Some(Duration::ZERO)
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
                offset: 0,
                limit: 2,
            },
        ]
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
