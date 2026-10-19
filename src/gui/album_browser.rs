use std::{
    cell::RefCell, collections::BTreeMap, ops::Range, path::PathBuf, rc::Rc, sync::Arc,
    time::Instant,
};

use gpui::{prelude::*, *};

use crate::{
    artwork::{Artwork as LoadedArtwork, ArtworkIdentity, ArtworkManager},
    core::CoreHandle,
    model::{AlbumKey, AlbumSort, Command, PlaybackStatus, Track},
    response::QueryRevisions,
};

use super::{
    artwork::{Artwork, ArtworkImages, ArtworkLoader, ArtworkView},
    components::{
        search::{new_search, set_search_counts},
        track::track_list_row,
    },
    format_time,
    list_interaction::{ContentDrag, ContentMenuRequest, TrackSource},
    scrolling::ScrollViews,
};
use rivu_ui::{
    ButtonTooltip, VirtualList, caption, column, compact_icon_button, icon_button, panel_surface,
    row,
    search::{SearchEvent, SearchField},
};

mod model;
use model::{AlbumModel, DataChange, Published, Request};

fn card_icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    hint: impl Into<SharedString>,
    cx: &mut Context<AlbumBrowser>,
    action: impl Fn(&mut AlbumBrowser, &mut Window, &mut Context<AlbumBrowser>) + 'static,
) -> Stateful<Div> {
    let theme = rivu_ui::theme::palette(cx);
    compact_icon_button(theme, icon_button(id, icon, hint, cx, action))
        .h_8()
        .rounded_none()
        .border_0()
        .opacity(0.)
        .group_hover("album-card", |style| style.opacity(1.))
        .focus(|style| style.opacity(1.).border_0())
        .bg(gpui::transparent_black())
        .text_color(rgb(theme.muted))
        .hover(|style| {
            style
                .bg(gpui::transparent_black())
                .text_color(rgb(theme.text))
        })
}

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
    panel_id: u64,
    scroll_views: Rc<RefCell<ScrollViews>>,
    handle: CoreHandle,
    snapshots: Rc<super::feature::SnapshotSource>,
    i18n: Arc<crate::i18n::Localizer>,
    localized_text: BTreeMap<&'static str, SharedString>,
    manager: Arc<ArtworkManager>,
    images: ArtworkImages,
    default_album: Entity<Artwork>,
    model: AlbumModel,
    loading_text: rivu_ui::loading::LoadingText<()>,
    error: Option<String>,
    dismissed_actions: Option<AlbumKey>,
    covers: BTreeMap<usize, AlbumCover>,
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
        default_album: Entity<Artwork>,
        images: ArtworkImages,
        cx: &mut Context<Self>,
    ) -> Self {
        let handle = context.handle().clone();
        let i18n = context.i18n;
        let scroll_views = context.scroll;
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
        let album_search = cx.new(|cx| new_search(&i18n, cx));
        let tracks_search = cx.new(|cx| new_search(&i18n, cx));
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
            default_album,
            model: AlbumModel::default(),
            loading_text: rivu_ui::loading::LoadingText::new(std::time::Duration::from_millis(150)),
            error: None,
            dismissed_actions: None,
            covers: BTreeMap::new(),
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
            .expect("cached album text")
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
        let _ = self.loader.prepare(None);
        for cover in std::mem::take(&mut self.covers).into_values() {
            cover.view.replace(None, cx);
        }
    }

    fn apply_data_change(&mut self, change: DataChange, cx: &mut App) {
        if change.albums {
            self.album_search
                .update(cx, |search, cx| search.clear_counts(cx));
            self.clear_covers(cx);
        }
        if change.tracks {
            self.tracks_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
        if (self.model.selected().is_none() && change.albums)
            || (self.model.selected().is_some() && change.tracks)
        {
            self.loading_text.clear();
        }
    }

    fn reload(&mut self, change: DataChange, cx: &mut Context<Self>) {
        self.loading_text.clear();
        self.clear_covers(cx);
        self.apply_data_change(change, cx);
        if !self.model.visible() {
            return;
        }
        if self.model.selected().is_none() {
            self.restore_grid_anchor();
        }
        self.error = None;
        self.request_next(cx);
    }

    fn request_next(&mut self, cx: &mut Context<Self>) {
        let Some(Request {
            query,
            count_query,
            generation,
            ticket,
        }) = self.model.prepare()
        else {
            return;
        };
        self.loading_text.begin((), generation, Instant::now());
        self.loading_text
            .schedule(cx, |this| &mut this.loading_text);
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let worker_query = query.clone();
            let (response, count_response) = super::query::run(
                handle,
                cx,
                move |handle| {
                    let response = handle.query_if_current(worker_query, ticket.clone());
                    let count = response
                        .result
                        .is_ok()
                        .then_some(count_query)
                        .flatten()
                        .map(|query| {
                            let response = handle.query_if_current(query.clone(), ticket);
                            (query, response)
                        });
                    (response, count)
                },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.loading_text.finish(&(), generation);
                        this.loading_text
                            .schedule(cx, |this| &mut this.loading_text);
                        let revisions = this.snapshots.read().query_revisions(None);
                        let data = this.model.timeout(&query, generation, response, revisions);
                        this.apply_data_change(data, cx);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.loading_text.finish(&(), generation);
                this.loading_text
                    .schedule(cx, |this| &mut this.loading_text);
                let revisions = this.snapshots.read().query_revisions(None);
                if this.model.albums_need_refresh(revisions) {
                    this.capture_grid_anchor();
                }
                let completion = this.model.complete(
                    &query,
                    generation,
                    response,
                    count_response,
                    revisions,
                    this.i18n.text("gl-album-response"),
                );
                this.apply_data_change(completion.data, cx);
                if let Some((matches, total)) = completion.counts {
                    let search = match completion.published {
                        Published::Albums => &this.album_search,
                        Published::Tracks => &this.tracks_search,
                        Published::None => unreachable!(),
                    };
                    search.update(cx, |search, cx| {
                        set_search_counts(search, matches, total, &this.i18n, cx)
                    });
                }
                if completion.published == Published::Albums {
                    this.prepare_cover(cx);
                }
                this.request_next(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn retry(&mut self, cx: &mut Context<Self>) {
        self.model.retry();
        self.request_next(cx);
        cx.notify();
    }

    fn loading_row(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let theme = rivu_ui::theme::palette(cx);
        let error = if self.model.selected().is_some() {
            self.model.tracks().error(index)
        } else {
            self.model.albums().error(index)
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
        self.demand = start..end;
        // Updating demand while a request is pending still records the latest viewport.
        self.model.demand_albums(self.demand.clone());
        self.prepare_cover(cx);
        self.request_next(cx);
    }

    fn prepare_cover(&mut self, cx: &mut Context<Self>) {
        if !self.model.visible() || self.model.selected().is_some() {
            return;
        }
        for (_, cover) in self.covers.extract_if(.., |index, _| {
            !self.demand.contains(index) || self.model.albums().get(*index).is_none()
        }) {
            cover.view.replace(None, cx);
        }
        let mut changed = false;
        for index in self.demand.clone() {
            let Some(album) = self.model.albums().get(index) else {
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
            let track = &self.model.albums().get(index)?.representative;
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
                    if !this.model.visible() || this.model.selected().is_some() {
                        return;
                    }
                    let mut changed = false;
                    for index in this.demand.clone() {
                        let current = this
                            .model
                            .albums()
                            .get(index)
                            .map(|album| &album.representative);
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
        let theme = rivu_ui::theme::palette(cx);
        let Some(album) = self.model.albums().get(index) else {
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
        let hover_key = key.clone();
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
            .right_2()
            .bottom_2()
            .gap_0()
            .when(self.dismissed_actions.as_ref() == Some(&key), |view| {
                view.invisible()
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(card_icon_button(
                ("play-album", index),
                "",
                self.tr("gl-play-album"),
                cx,
                move |this, window, cx| {
                    this.dismissed_actions = Some(play_key.clone());
                    cx.notify();
                    window.blur(cx);
                    cx.emit(AlbumPlayRequest(play_key.clone()));
                },
            ))
            .child(card_icon_button(
                ("enqueue-album", index),
                "󰐕",
                self.tr("gl-enqueue-album"),
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
            .on_hover(cx.listener(move |this, hovered, _, cx| {
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
                    rivu_ui::begin_list_drag(window, cx);
                    cx.new(|_| rivu_ui::ListDragPreview {
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
            .child(actions)
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

fn track_marker(current: Option<i64>, status: PlaybackStatus, id: i64) -> Option<&'static str> {
    (current == Some(id)).then_some(match status {
        PlaybackStatus::Playing => "gl-playing",
        PlaybackStatus::Paused => "gl-paused",
        PlaybackStatus::Stopped => "gl-current",
    })
}

impl Render for AlbumBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scroll_views.borrow_mut().begin_panel(self.panel_id);
        let theme = rivu_ui::theme::palette(cx);
        let total = self.model.total();
        let mut header = row().items_end().flex_shrink_0().min_w_0();
        if let Some(key) = self.model.selected() {
            let play_key = key.clone();
            let enqueue_key = key.clone();
            let menu_key = key.clone();
            header = header
                .child(rivu_ui::quiet_icon_button(
                    theme,
                    icon_button(
                        "albums-back",
                        "",
                        self.tr("gl-back-albums"),
                        cx,
                        |this, _, cx| {
                            let change = this.model.select(None);
                            this.reload(change.data, cx);
                        },
                    ),
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
                    move |_, _, cx| {
                        cx.emit(AlbumPlayRequest(play_key.clone()));
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
                    rivu_ui::list_panel_header(
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
                    .min_w_12(),
                )
                .child(self.album_search.clone())
                .child(icon_button(
                    "albums-sort",
                    match self.model.sort() {
                        AlbumSort::Title => "󰉿",
                        AlbumSort::Artist => "󰀄",
                    },
                    match self.model.sort() {
                        AlbumSort::Title => self.tr("gl-album-sort-title"),
                        AlbumSort::Artist => self.tr("gl-album-sort-artist"),
                    },
                    cx,
                    |this, _, cx| {
                        let change = this.model.toggle_sort();
                        this.reset_grid();
                        this.reload(change, cx);
                    },
                ));
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
                        range
                            .map(|index| {
                                let Some(track) = this.model.tracks().get(index) else {
                                    return this
                                        .loading_row(index, cx)
                                        .h(rivu_ui::list_row_height(window, cx))
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
                                .h(rivu_ui::list_row_height(window, cx))
                                .group("album-track-row")
                                .relative()
                                .gap_0()
                                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                    if event.click_count() == 2 {
                                        this.send(Command::Play { track_id: id }, cx);
                                    }
                                }))
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
                                    .font_features(super::appearance::fonts::tabular_numbers())
                                    .w_16()
                                    .flex_shrink_0()
                                    .text_right(),
                                )
                                .child(
                                    rivu_ui::hover_actions(theme, "album-track-row", false)
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
                                        .child(compact_icon_button(
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
                                        )),
                                )
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
    use super::{AlbumCover, anchored_index};
    use crate::{
        artwork::{
            ArtworkManager,
            tests::{tagged_wav, track},
        },
        gui::artwork::ArtworkImages,
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
}
