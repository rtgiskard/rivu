use std::{ops::Range, sync::Arc};

use gpui::{prelude::*, *};

use crate::{
    artwork::{ArtworkIdentity, ArtworkManager},
    core::CoreHandle,
    model::{
        AlbumKey, AlbumSort, AlbumSummary, Command, LibraryRow, PlaybackStatus, Query,
        SEARCH_DEBOUNCE,
    },
    response::ViewResponse,
};

use super::{
    BORDER, TEXT,
    artwork::{Artwork, ArtworkLoader, ArtworkView},
    button, caption, column,
    components::disabled_control,
    format_time, icon_button,
    input::{Input, InputEvent},
    paged, panel_surface, panel_toolbar, row,
};

const ALBUM_PAGE: usize = 24;

/// One bounded page and one sequential cover lookup per panel. Data requests
/// and cover completions are accepted only by their current generation/demand.
pub(super) struct AlbumBrowser {
    handle: CoreHandle,
    manager: Arc<ArtworkManager>,
    default_album: Entity<Artwork>,
    visible: bool,
    revision: u64,
    page_size: usize,
    generation: u64,
    request: Option<Task<()>>,
    loading: bool,
    error: Option<String>,
    selected: Option<AlbumKey>,
    album_offset: usize,
    track_offset: usize,
    total: usize,
    albums: Vec<AlbumSummary>,
    tracks: Vec<LibraryRow>,
    covers: Vec<ArtworkView>,
    retired: Vec<ArtworkView>,
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
        handle: CoreHandle,
        manager: Arc<ArtworkManager>,
        default_album: Entity<Artwork>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| Input::new("", "Search albums or artists", cx));
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
            handle,
            manager,
            default_album,
            visible: false,
            revision: 0,
            page_size: 1,
            generation: 0,
            request: None,
            loading: false,
            error: None,
            selected: None,
            album_offset: 0,
            track_offset: 0,
            total: 0,
            albums: Vec::new(),
            tracks: Vec::new(),
            covers: Vec::new(),
            retired: Vec::new(),
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

    pub(super) fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.visible && self.selected.is_none() && self.search.read(cx).is_focused(window)
    }

    pub(super) fn sync(
        &mut self,
        visible: bool,
        revision: u64,
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
        let changed =
            self.visible != visible || self.revision != revision || self.page_size != page_size;
        if !changed {
            return;
        }
        self.visible = visible;
        self.revision = revision;
        self.page_size = page_size.max(1);
        if visible {
            self.load(cx);
        } else {
            self.cancel(cx);
        }
        cx.notify();
    }

    fn clear_covers(&mut self, cx: &mut App) {
        self.demand = 0..0;
        let _ = self.loader.prepare(None);
        for cover in self.covers.drain(..) {
            cover.replace(None, cx);
            self.retired.push(cover);
        }
    }

    fn cancel(&mut self, cx: &mut App) {
        self.capture_grid_anchor();
        self.generation = self.generation.wrapping_add(1);
        self.request = None;
        self.loading = false;
        self.clear_covers(cx);
        self.albums.clear();
        self.tracks.clear();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.load_with_delay(false, cx);
    }

    fn load_with_delay(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.cancel(cx);
        if !self.visible {
            return;
        }
        self.loading = true;
        self.error = None;
        let generation = self.generation;
        let revision = self.revision;
        let handle = self.handle.clone();
        let debounce = debounce && self.query.is_some();
        let query = match &self.selected {
            Some(key) => Query::AlbumTracks {
                key: key.clone(),
                offset: self.track_offset,
                limit: self.page_size,
            },
            None => Query::Albums {
                query: self.query.clone(),
                sort: self.sort,
                offset: self.album_offset,
                limit: ALBUM_PAGE,
            },
        };
        self.request = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
                if !this
                    .update(cx, |this, _| this.visible && this.generation == generation)
                    .unwrap_or(false)
                {
                    return;
                }
            }
            let response = cx
                .background_executor()
                .spawn(async move { handle.query(query) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.visible || generation != this.generation || revision != this.revision {
                    return;
                }
                // A newer core snapshot may be waiting in the window wakeup queue.
                let current_revision = this.handle.gui_snapshot().library.revision;
                if response.revisions.library != revision || current_revision != revision {
                    this.revision = current_revision;
                    this.load(cx);
                    cx.notify();
                    return;
                }
                this.loading = false;
                match response.result {
                    Ok(ViewResponse::Albums(page)) => {
                        this.total = page.total;
                        this.albums = page.rows;
                        this.covers = (0..this.albums.len())
                            .map(|_| ArtworkView::default())
                            .collect();
                        this.restore_grid_anchor();
                    }
                    Ok(ViewResponse::AlbumTracks(page)) => {
                        this.total = page.total;
                        this.tracks = page.rows;
                    }
                    Err(error) => this.error = Some(error.to_string()),
                    _ => this.error = Some("Unexpected album query response".into()),
                }
                let offset = if this.selected.is_some() {
                    &mut this.track_offset
                } else {
                    &mut this.album_offset
                };
                if *offset >= this.total && *offset != 0 {
                    *offset = 0;
                    if this.selected.is_some() {
                        this.track_scroll
                            .scroll_to_item_strict(0, ScrollStrategy::Top);
                    } else {
                        this.reset_grid();
                    }
                    this.load(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(error) = self.handle.send(command) {
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn open(&mut self, key: AlbumKey, cx: &mut Context<Self>) {
        self.capture_grid_anchor();
        if self.selected.as_ref() != Some(&key) {
            self.track_offset = 0;
            self.track_scroll
                .scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.selected = Some(key);
        self.load(cx);
    }

    fn reset_grid(&mut self) {
        self.album_offset = 0;
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
            .and_then(|key| self.albums.iter().position(|album| &album.key == key));
        let index = anchored_index(previous, grid_row, self.columns, self.albums.len());
        self.grid_anchor = Some(self.albums[index].key.clone());
        self.grid_anchor_index = index;
    }

    fn restore_grid_anchor(&mut self) {
        let index = self
            .grid_anchor
            .as_ref()
            .and_then(|key| self.albums.iter().position(|album| &album.key == key))
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
        let next = start..end;
        if self.demand != next {
            for index in self.demand.clone() {
                if !next.contains(&index) {
                    self.covers[index].replace(None, cx);
                }
            }
            self.demand = next;
        }
        self.prepare_cover(cx);
    }

    fn prepare_cover(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for index in self.demand.clone() {
            changed |= self.covers[index].replace(
                self.manager
                    .cached(&self.albums[index].representative)
                    .filter(|artwork| !artwork.is_placeholder()),
                cx,
            );
        }
        let next = self.demand.clone().find_map(|index| {
            let track = &self.albums[index].representative;
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

    pub(super) fn retire_images(&mut self, window: &mut Window) {
        for cover in self.retired.drain(..).chain(self.covers.iter().cloned()) {
            cover.release_retired(window);
        }
    }

    pub(super) fn release_all_images(&mut self, window: &mut Window, cx: &mut App) {
        for cover in self.retired.drain(..).chain(self.covers.iter().cloned()) {
            cover.release_all_images(window, cx);
        }
    }

    fn card(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let album = &self.albums[index];
        let key = album.key.clone();
        let play_key = key.clone();
        let enqueue_key = key.clone();
        let image = self.covers[index].image(cx);
        let directory: SharedString = key.directory.to_string_lossy().into_owned().into();
        column()
            .id(("album-card", index))
            .flex_1()
            .min_w_0()
            .h_full()
            .p_2()
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
                button(
                    ("open-album", index),
                    album_title(&key),
                    cx,
                    move |this, _, cx| this.open(key.clone(), cx),
                )
                .w_full()
                .overflow_hidden()
                .tooltip(move |_, cx| {
                    cx.new(|_| super::ButtonTooltip {
                        text: directory.clone(),
                    })
                    .into()
                }),
            )
            .child(caption(album.artist.clone()).truncate())
            .child(caption(format!("{} tracks", album.track_count)))
            .child(
                panel_toolbar()
                    .child(icon_button(
                        ("play-album", index),
                        "",
                        "Play album",
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
                        ("enqueue-album", index),
                        "󰐕",
                        "Add album to queue",
                        cx,
                        move |this, _, cx| {
                            this.send(
                                Command::EnqueueAlbum {
                                    key: enqueue_key.clone(),
                                },
                                cx,
                            )
                        },
                    )),
            )
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
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Stopped => "Current",
    })
}

fn album_title(key: &AlbumKey) -> String {
    if key.title.is_empty() {
        "Unknown album".into()
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
                    "Back to albums",
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
                        .child(album_title(key)),
                )
                .child(icon_button(
                    "album-play-all",
                    "",
                    "Play album",
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
                    "Add album to queue",
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
                .child(div().text_lg().child("Albums"))
                .child(div().flex_1().min_w_0().child(self.search.clone()))
                .child(button(
                    "albums-sort",
                    match self.sort {
                        AlbumSort::Title => "Sort: Title",
                        AlbumSort::Artist => "Sort: Artist",
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
        let offset = if self.selected.is_some() {
            self.track_offset
        } else {
            self.album_offset
        };
        let limit = if self.selected.is_some() {
            self.page_size
        } else {
            ALBUM_PAGE
        };
        let pager = panel_toolbar()
            .child(disabled_control(
                icon_button(
                    "albums-previous",
                    "",
                    "Previous page",
                    cx,
                    |this, _, cx| {
                        let offset = if this.selected.is_some() {
                            this.track_offset
                        } else {
                            this.album_offset
                        };
                        if this.loading || offset == 0 {
                            return;
                        }
                        if this.selected.is_some() {
                            this.track_offset = this.track_offset.saturating_sub(this.page_size);
                            this.track_scroll
                                .scroll_to_item_strict(0, ScrollStrategy::Top);
                        } else {
                            this.album_offset = this.album_offset.saturating_sub(ALBUM_PAGE);
                            this.reset_grid_scroll();
                        }
                        this.load(cx);
                    },
                ),
                self.loading || offset == 0,
            ))
            .child(caption(if self.total == 0 {
                "0 items".into()
            } else {
                format!(
                    "{}–{} of {}",
                    offset + 1,
                    (offset + limit).min(self.total),
                    self.total
                )
            }))
            .child(disabled_control(
                icon_button("albums-next", "", "Next page", cx, |this, _, cx| {
                    let offset = if this.selected.is_some() {
                        this.track_offset
                    } else {
                        this.album_offset
                    };
                    let limit = if this.selected.is_some() {
                        this.page_size
                    } else {
                        ALBUM_PAGE
                    };
                    if this.loading || offset.saturating_add(limit) >= this.total {
                        return;
                    }
                    if this.selected.is_some() {
                        this.track_offset += this.page_size;
                        this.track_scroll
                            .scroll_to_item_strict(0, ScrollStrategy::Top);
                    } else {
                        this.album_offset += ALBUM_PAGE;
                        this.reset_grid_scroll();
                    }
                    this.load(cx);
                }),
                self.loading || offset.saturating_add(limit) >= self.total,
            ));
        let content = if self.loading {
            column().child(caption("Loading…")).into_any_element()
        } else if let Some(error) = &self.error {
            column()
                .child(caption(error.clone()))
                .child(icon_button(
                    "albums-retry",
                    "󰑐",
                    "Retry loading albums",
                    cx,
                    |this, _, cx| this.load(cx),
                ))
                .into_any_element()
        } else if self.selected.is_some() {
            if self.tracks.is_empty() {
                column()
                    .child(caption("No tracks in this album"))
                    .into_any_element()
            } else {
                uniform_list(
                    "album-tracks",
                    self.tracks.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let track = &this.tracks[index];
                                let id = track.id;
                                let marker =
                                    track_marker(this.current_track, this.playback_status, id);
                                row()
                                    .h(rems(3.5))
                                    .w_full()
                                    .p_2()
                                    .group("album-track-row")
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .child(
                                        column()
                                            .flex_1()
                                            .overflow_hidden()
                                            .gap_0()
                                            .child(div().truncate().child(track.title.clone()))
                                            .child(caption(track.artist.clone()).truncate()),
                                    )
                                    .when(track.missing, |row| row.child(caption("Missing")))
                                    .when_some(marker, |row, marker| row.child(caption(marker)))
                                    .child(caption(
                                        track.duration.map_or_else(|| "—".into(), format_time),
                                    ))
                                    .child(
                                        row()
                                            .gap_0()
                                            .invisible()
                                            .group_hover("album-track-row", |style| style.visible())
                                            .child(icon_button(
                                                ("album-track-play", id as u64),
                                                "",
                                                "Play track",
                                                cx,
                                                move |this, _, cx| {
                                                    this.send(Command::Play { track_id: id }, cx)
                                                },
                                            ))
                                            .child(icon_button(
                                                ("album-track-add", id as u64),
                                                "󰐕",
                                                "Add track to queue",
                                                cx,
                                                move |this, _, cx| {
                                                    this.send(
                                                        Command::Enqueue {
                                                            track_ids: vec![id],
                                                        },
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.track_scroll)
                .size_full()
                .into_any_element()
            }
        } else if self.albums.is_empty() {
            column()
                .child(caption(if self.query.is_some() {
                    "No albums match your search"
                } else {
                    "No albums in the library"
                }))
                .into_any_element()
        } else {
            let columns = self.columns;
            let generation = self.generation;
            let demand_owner = cx.weak_entity();
            let measure_owner = cx.weak_entity();
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
                                    let mut cards =
                                        row().w_full().h(rems(17.)).items_start().pb_2();
                                    for index in start..end {
                                        cards = cards.child(this.card(index, cx));
                                    }
                                    for _ in end..start + columns {
                                        cards = cards.child(div().flex_1().min_w_0());
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
                                if generation == this.generation && columns == this.columns {
                                    this.cover_demand(range, cx);
                                }
                            });
                        });
                    }))
                    .size_full(),
                )
                .into_any_element()
        };
        panel_surface("albums-browser")
            .p_2()
            .child(header)
            .child(div().flex_1().min_h_0().min_w_0().child(content))
            .child(pager)
    }
}

#[cfg(test)]
mod tests {
    use super::anchored_index;

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
