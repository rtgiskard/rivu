use super::{Library, LibraryEvent, TreeBranch, tree::TreeItem};
use crate::{
    model::{DirectoryRow, LibraryLocateResult, Query},
    projection::QueryProjection,
    response::{QueryRevisions, ViewResponse},
};
use gpui::{Context, ScrollStrategy, Window};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(in crate::gui) struct LibraryLocateState {
    projection: QueryProjection,
    // Logical cancellation never releases the physical ancestor worker.
    pub(in crate::gui) pending: bool,
    pub(super) loading: bool,
    queued: bool,
    pub(in crate::gui) notice: Option<(i64, LibraryLocateResult)>,
    pub(in crate::gui) reveal: Option<(i64, TreeBranch, usize)>,
    pub(in crate::gui) reveal_scrolled: bool,
    pub(in crate::gui) highlight: Option<(i64, Instant)>,
    pub(in crate::gui) highlight_task: Option<gpui::Task<()>>,
}

impl LibraryLocateState {
    pub(in crate::gui) fn invalidate(&mut self) {
        self.projection.cancel();
        self.loading = false;
        self.notice = None;
        self.reveal = None;
        self.reveal_scrolled = false;
        self.highlight = None;
        self.highlight_task = None;
    }

    fn sync(&mut self, query: &Query, revisions: QueryRevisions) {
        if self.projection.sync(query, revisions) {
            self.invalidate();
        }
    }
}

// Ancestor queries share the navigation request's freshness contract; their path
// and range endpoints are validated by the locate traversal, not page demand.
struct Request {
    generation: u64,
    query: Query,
    track_id: i64,
}

impl Request {
    fn current(&self, app: &Library) -> bool {
        app.library.locate.projection.accepts(
            &self.query,
            self.generation,
            app.state.query_revisions(None),
        ) && app
            .state
            .current_track()
            .is_some_and(|track| track.id == self.track_id)
    }

    fn accepts(&self, app: &Library, revisions: QueryRevisions) -> bool {
        self.current(app)
            && app
                .library
                .locate
                .projection
                .accepts(&self.query, self.generation, revisions)
    }
}

impl Library {
    pub(in crate::gui) fn locate_playing_library(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_library_locate(cx);
    }

    fn begin_library_locate(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.state.current_track() else {
            return;
        };
        let track_id = track.id;
        let ancestry_limit = track.path.components().count();
        let root = self
            .state
            .system
            .config
            .library_roots
            .iter()
            .enumerate()
            .find(|(_, root)| track.path.starts_with(root))
            .map(|(index, root)| (index, root.clone()));
        cx.emit(LibraryEvent::Reveal);
        if !self.available {
            return;
        }
        if self.library.locate.pending {
            self.library.locate.invalidate();
            self.library.locate.queued = true;
            cx.notify();
            return;
        }
        self.library.locate.invalidate();
        let other_tracks = root.is_none();
        let (root_index, mut path) = root
            .map(|(index, path)| (Some(index), path))
            .unwrap_or_default();
        let revisions = self.state.query_revisions(None);
        let buffer = self.library.buffer();
        let query = buffer.query.clone();
        let favorite = buffer.favorite;
        let missing = buffer.missing;
        let sort = buffer.sort;
        let tree = self.library.tree_active();
        let locate_query = if other_tracks {
            Query::OtherTracksLocate {
                track_id,
                query: query.clone(),
                favorite,
                missing,
                sort,
            }
        } else {
            Query::LibraryLocate {
                track_id,
                path: path.clone(),
                tree,
                query: query.clone(),
                favorite,
                missing,
                sort,
            }
        };
        self.library.locate.sync(&locate_query, revisions);
        let request = Request {
            generation: self.library.locate.projection.generation(),
            query: locate_query,
            track_id,
        };
        let ticket = self.library.locate.projection.ticket();
        let handle = self.context.handle().clone();
        self.library.locate.pending = true;
        self.library.locate.loading = true;
        cx.spawn(async move |this, cx| {
            // One worker and one scalar query per ancestor; no full-library pages.
            let mut locations = Vec::new();
            loop {
                let query = if other_tracks {
                    Query::OtherTracksLocate {
                        track_id,
                        query: query.clone(),
                        favorite,
                        missing,
                        sort,
                    }
                } else {
                    Query::LibraryLocate {
                        track_id,
                        path,
                        tree,
                        query: query.clone(),
                        favorite,
                        missing,
                        sort,
                    }
                };
                let handle = handle.clone();
                let ticket = ticket.clone();
                let response = crate::gui::query::run(
                    handle,
                    cx,
                    move |handle| handle.query_if_current(query, ticket),
                    |response, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.refresh(cx);
                            if request.accepts(this, response.revisions) {
                                this.library.locate.invalidate();
                                if let Err(error) = response.result {
                                    this.report_error(error.to_string(), cx);
                                }
                                cx.notify();
                            }
                        });
                    },
                )
                .await;
                let current = this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        request.accepts(this, response.revisions)
                    })
                    .unwrap_or(false);
                if !current {
                    break;
                }
                match response.result {
                    Ok(ViewResponse::LibraryLocate(LibraryLocateResult::Located(location))) => {
                        let next = location.child.clone();
                        locations.push(location);
                        if let Some(next) = next {
                            if locations.len() >= ancestry_limit {
                                let _ = this.update(cx, |this, cx| {
                                    if !request.current(this) {
                                        return;
                                    }
                                    this.report_error(
                                        this.i18n.text("gl-locate-library-inconsistent").to_owned(),
                                        cx,
                                    );
                                    cx.notify();
                                });
                                break;
                            }
                            path = next;
                            continue;
                        }
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            let roots = &this.state.system.config.library_roots;
                            if let Some((branch, offset)) =
                                this.library.reveal_playing(root_index, roots, &locations)
                            {
                                this.library.locate.reveal =
                                    Some((track_id, branch.clone(), offset));
                                this.request_directory_page(branch, offset, cx);
                            }
                            cx.notify();
                        });
                        break;
                    }
                    Ok(ViewResponse::LibraryLocate(result)) => {
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            this.library.locate.notice = Some((track_id, result));
                            cx.notify();
                        });
                        break;
                    }
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            this.report_error(error.to_string(), cx);
                            cx.notify();
                        });
                        break;
                    }
                    _ => {
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            this.report_error(
                                this.i18n.text("gl-locate-library-response").to_owned(),
                                cx,
                            );
                            cx.notify();
                        });
                        break;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.library.locate.pending = false;
                this.library.locate.loading = false;
                if std::mem::take(&mut this.library.locate.queued) {
                    this.begin_library_locate(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::gui) fn clear_filters_and_locate_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.library.locate.notice.as_ref().map(|(id, _)| *id);
        if target != self.state.current_track().map(|track| track.id) || target.is_none() {
            self.library.locate.notice = None;
            cx.notify();
            return;
        }
        self.library_search
            .update(cx, |search, cx| search.clear(cx));
        if self.library.buffer().query.is_some() {
            self.library.clear_selection();
        }
        self.library.clear_locate_filters();
        self.request_directory_page(TreeBranch::default(), 0, cx);
        self.locate_playing_library(window, cx);
    }

    pub(in crate::gui) fn finish_library_locate_render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((track_id, branch, offset)) = self.library.locate.reveal.clone() else {
            return;
        };
        let Some(index) = self.library.tree().location_index(&branch, offset) else {
            return;
        };
        if self
            .state
            .current_track()
            .is_none_or(|track| track.id != track_id)
        {
            self.library.locate.reveal = None;
            return;
        }
        let ready = matches!(self.library.tree().item(index), Some(TreeItem::Entry(row))
            if matches!(&row.row, DirectoryRow::Track(track) if track.id == track_id));
        if !ready && self.library.locate.reveal_scrolled {
            return;
        }
        let generation = self.library.locate.projection.generation();
        let entity = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let _ =
                entity.update(cx, |this, cx| {
                    if this.library.locate.projection.generation() == generation
                        && this
                            .state
                            .current_track()
                            .is_some_and(|track| track.id == track_id)
                        && this.library.locate.reveal.as_ref().is_some_and(
                            |(id, target, position)| {
                                *id == track_id && target == &branch && *position == offset
                            },
                        )
                    {
                        this.library.locate.reveal_scrolled = true;
                        this.library
                            .tree()
                            .scroll
                            .scroll_to_item(index, ScrollStrategy::Nearest);
                        if ready {
                            this.library.locate.reveal = None;
                            this.library.locate.highlight = Some((track_id, Instant::now()));
                            this.library.locate.highlight_task =
                                Some(cx.spawn(async move |this, cx| {
                                    cx.background_executor().timer(Duration::from_secs(2)).await;
                                    let _ = this.update(cx, |this, cx| {
                                        if this.library.locate.projection.generation() == generation
                                        {
                                            this.library.locate.highlight = None;
                                            cx.notify();
                                        }
                                    });
                                }));
                        }
                        cx.notify();
                    }
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LibrarySort;

    fn query() -> Query {
        Query::LibraryLocate {
            track_id: 7,
            path: "/music".into(),
            tree: true,
            query: Some("needle".into()),
            favorite: None,
            missing: None,
            sort: LibrarySort::Album,
        }
    }

    fn revisions() -> QueryRevisions {
        QueryRevisions {
            library: 3,
            structure: 5,
            ..Default::default()
        }
    }

    #[test]
    fn locate_freshness_uses_only_its_authoritative_query_stamp() {
        let mut state = LibraryLocateState::default();
        let query = query();
        state.sync(&query, revisions());
        let generation = state.projection.generation();
        let mut current = revisions();
        current.playlist = 11;
        current.queue = 13;
        current.duplicate_groups = 17;
        current.duplicate_members = 19;
        state.sync(&query, current);
        assert_eq!(state.projection.generation(), generation);
        assert!(state.projection.accepts(&query, generation, current));
        current.library += 1;
        assert!(!state.projection.accepts(&query, generation, current));
        current = revisions();
        current.structure += 1;
        assert!(!state.projection.accepts(&query, generation, current));
    }

    #[test]
    fn cancelled_locate_keeps_the_physical_gate_and_retires_navigation_only() {
        let mut state = LibraryLocateState::default();
        let query = query();
        state.sync(&query, revisions());
        let generation = state.projection.generation();
        state.pending = true;
        state.notice = Some((7, LibraryLocateResult::OutsideRoot));
        state.reveal = Some((7, TreeBranch::default(), 2));
        state.invalidate();
        assert!(state.pending);
        assert!(state.notice.is_none());
        assert!(state.reveal.is_none());
        assert!(!state.projection.accepts(&query, generation, revisions()));
        // Only the worker completion releases the slot; another explicit locate
        // can then reuse the same identity with a new submission generation.
        state.pending = false;
        state.sync(&query, revisions());
        assert!(
            state
                .projection
                .accepts(&query, state.projection.generation(), revisions(),)
        );
    }

    #[test]
    fn locate_filter_or_target_change_rejects_the_previous_traversal() {
        let mut state = LibraryLocateState::default();
        let original = query();
        state.sync(&original, revisions());
        let generation = state.projection.generation();
        let mut changed = original.clone();
        let Query::LibraryLocate {
            query, track_id, ..
        } = &mut changed
        else {
            unreachable!()
        };
        *query = None;
        *track_id = 9;
        state.sync(&changed, revisions());
        assert!(!state.projection.accepts(&original, generation, revisions()));
        assert!(
            state
                .projection
                .accepts(&changed, state.projection.generation(), revisions(),)
        );
    }
}
