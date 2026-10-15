use super::{TreeBranch, tree::TreeItem};
use crate::{
    gui::{Field, GuiApp},
    model::{DirectoryRow, LibraryLocateResult, Query},
    response::ViewResponse,
};
use gpui::{Context, ScrollStrategy, Window};
use std::time::{Duration, Instant};

// The worker owns only navigation intent. Selection and Details never participate.
// Projection changes invalidate the locate generation; ordinary page arrivals do not.
#[derive(Clone, Copy)]
struct Request {
    generation: u64,
    track_id: i64,
    library: u64,
    structure: u64,
}

impl Request {
    fn current(self, app: &GuiApp) -> bool {
        app.browser.library.locate.generation == self.generation
            && app
                .state
                .current_track()
                .is_some_and(|track| track.id == self.track_id)
            && app.state.library.revision == self.library
            && app.state.library.structure_revision == self.structure
    }
}

impl GuiApp {
    pub(in crate::gui) fn locate_playing_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.activate_library(window, cx, false);
        if !self
            .layouts
            .current()
            .panels()
            .iter()
            .any(|panel| panel.kind == "library")
            || self.browser.library.locate.pending
        {
            return;
        }
        self.browser.library.locate.invalidate();
        let Some((root_index, mut path)) = root else {
            self.browser.library.locate.notice = Some((track_id, LibraryLocateResult::OutsideRoot));
            cx.notify();
            return;
        };
        let request = Request {
            generation: self.browser.library.locate.generation,
            track_id,
            library: self.state.library.revision,
            structure: self.state.library.structure_revision,
        };
        let buffer = self.browser.library.buffer();
        let query = buffer.query.clone();
        let favorite = buffer.favorite;
        let missing = buffer.missing;
        let sort = buffer.sort;
        let tree = self.browser.library.tree_active();
        let handle = self.handle.clone();
        self.browser.library.locate.pending = true;
        cx.spawn(async move |this, cx| {
            // One worker and one scalar query per ancestor; no full-library pages.
            let mut locations = Vec::new();
            loop {
                let query = Query::LibraryLocate {
                    track_id,
                    path,
                    tree,
                    query: query.clone(),
                    favorite,
                    missing,
                    sort,
                };
                let handle = handle.clone();
                let response = cx
                    .background_executor()
                    .spawn(async move { handle.query(query) })
                    .await;
                let current = this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        request.current(this)
                            && response.revisions.library == request.library
                            && response.revisions.structure == request.structure
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
                                    this.error = Some(
                                        this.i18n.text("gl-locate-library-inconsistent").to_owned(),
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
                            if let Some((branch, offset)) = this
                                .browser
                                .library
                                .reveal_playing(root_index, roots, &locations)
                            {
                                this.browser.library.locate.reveal =
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
                            this.browser.library.locate.notice = Some((track_id, result));
                            cx.notify();
                        });
                        break;
                    }
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            this.error = Some(error.to_string());
                            cx.notify();
                        });
                        break;
                    }
                    _ => {
                        let _ = this.update(cx, |this, cx| {
                            if !request.current(this) {
                                return;
                            }
                            this.error =
                                Some(this.i18n.text("gl-locate-library-response").to_owned());
                            cx.notify();
                        });
                        break;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.browser.library.locate.pending = false;
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
        let target = self
            .browser
            .library
            .locate
            .notice
            .as_ref()
            .map(|(id, _)| *id);
        if target != self.state.current_track().map(|track| track.id) || target.is_none() {
            self.browser.library.locate.notice = None;
            cx.notify();
            return;
        }
        self.set_value(Field::Search, "", cx);
        self.browser.library.clear_locate_filters();
        self.request_directory_page(TreeBranch::default(), 0, cx);
        self.locate_playing_library(window, cx);
    }

    pub(in crate::gui) fn finish_library_locate_render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((track_id, branch, offset)) = self.browser.library.locate.reveal.clone() else {
            return;
        };
        let Some(index) = self.browser.library.tree().location_index(&branch, offset) else {
            return;
        };
        if self
            .state
            .current_track()
            .is_none_or(|track| track.id != track_id)
        {
            self.browser.library.locate.reveal = None;
            return;
        }
        let ready = matches!(self.browser.library.tree().item(index), Some(TreeItem::Entry(row))
            if matches!(&row.row, DirectoryRow::Track(track) if track.id == track_id));
        if !ready && self.browser.library.locate.reveal_scrolled {
            return;
        }
        let generation = self.browser.library.locate.generation;
        let entity = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let _ = entity.update(cx, |this, cx| {
                if this.browser.library.locate.generation == generation
                    && this
                        .state
                        .current_track()
                        .is_some_and(|track| track.id == track_id)
                    && this.browser.library.locate.reveal.as_ref().is_some_and(
                        |(id, target, position)| {
                            *id == track_id && target == &branch && *position == offset
                        },
                    )
                {
                    this.browser.library.locate.reveal_scrolled = true;
                    this.browser
                        .library
                        .tree()
                        .scroll
                        .scroll_to_item(index, ScrollStrategy::Nearest);
                    if ready {
                        this.browser.library.locate.reveal = None;
                        this.browser.library.locate.highlight = Some((track_id, Instant::now()));
                        this.browser.library.locate.highlight_task =
                            Some(cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(Duration::from_secs(2)).await;
                                let _ = this.update(cx, |this, cx| {
                                    if this.browser.library.locate.generation == generation {
                                        this.browser.library.locate.highlight = None;
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
