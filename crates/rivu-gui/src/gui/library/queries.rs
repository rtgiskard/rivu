//! Library-owned query dispatch; worker retirement is independent of visibility.
use super::{
    Library, LibraryEvent, TreeBranch,
    model::{self, CountBlock, SEARCH_COUNT_QUERY},
};
use crate::gui::components::search::set_search_count_parts;
use gpui::Context;
use rivu_protocol::{
    model::{LibrarySort, Query},
    response::{QueryError, ViewResponse},
};
use std::rc::Rc;

async fn library_match_count(
    handle: &crate::frontend::FrontendHandle,
    request: &model::SearchCountRequest,
    query: Query,
) -> Result<usize, CountBlock> {
    let response = handle.query_if_current(query, request.ticket.clone()).await;
    match response.result {
        Ok(ViewResponse::LibraryPage(page))
            if SEARCH_COUNT_QUERY.stamp(response.revisions) == request.stamp =>
        {
            Ok(page.total)
        }
        Ok(ViewResponse::LibraryPage(_))
        | Err(QueryError::RevisionChanged | QueryError::Cancelled) => Err(CountBlock::Retired),
        _ => Err(CountBlock::Failed),
    }
}

async fn library_search_counts(
    handle: &crate::frontend::FrontendHandle,
    request: &model::SearchCountRequest,
) -> Result<model::SearchCountResult, CountBlock> {
    let model::SearchCountRequest {
        scope,
        ticket,
        other_empty,
        ..
    } = request;
    let model::SearchCountScope {
        roots,
        query,
        favorite,
        missing,
    } = scope.as_ref();
    if !ticket.is_current() {
        return Err(CountBlock::Retired);
    }
    let mut matched = 0;
    let mut root_counts = std::collections::HashMap::new();
    for root in roots {
        let total = library_match_count(
            handle,
            request,
            Query::LibraryRootPage {
                path: root.clone(),
                query: query.clone(),
                favorite: *favorite,
                missing: *missing,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 0,
            },
        )
        .await?;
        matched += total;
        root_counts.insert(root.clone(), total);
    }
    let other_matched = if *other_empty {
        0
    } else {
        library_match_count(
            handle,
            request,
            Query::OtherTracksPage {
                query: query.clone(),
                favorite: *favorite,
                missing: *missing,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 0,
            },
        )
        .await?
    };
    Ok(model::SearchCountResult {
        matched: matched + other_matched,
        roots: root_counts,
        other_matched,
    })
}

impl Library {
    pub(in crate::gui) fn request_root_counts(&mut self, cx: &mut Context<Self>) {
        if !self.demanded() {
            return;
        }
        self.request_library_search_counts(cx);
        self.request_other_count(cx);
        let state = Rc::clone(&self.state);
        let Some(read) = self.prepare_root_count_read(
            &state.system.config.library_roots,
            state.library.structure_revision,
            cx.background_executor().now(),
        ) else {
            return;
        };
        let handle = self.context.handle().clone();
        let query = read.query.clone();
        let ticket = read.ticket.clone();
        cx.spawn(async move |this, cx| {
            let mut timed_out = false;
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(query, ticket).await },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        let state = Rc::clone(&this.state);
                        if let Some(error) = this.publish_root_count_read(
                            &read,
                            &state.system.config.library_roots,
                            state.query_revisions(None),
                            response,
                        ) {
                            this.report_error(error, cx);
                        }
                        this.schedule_refresh_wake(cx);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                let state = Rc::clone(&this.state);
                if timed_out {
                    this.retire_root_count_read(&read);
                } else if let Some(error) = this.complete_root_count_read(
                    read,
                    &state.system.config.library_roots,
                    state.query_revisions(None),
                    response,
                ) {
                    this.report_error(error, cx);
                }
                this.request_root_counts(cx);
                this.schedule_refresh_wake(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn request_other_count(&mut self, cx: &mut Context<Self>) {
        let Some(read) = self.library.prepare_other_count() else {
            return;
        };
        let handle = self.context.handle().clone();
        let ticket = read.ticket.clone();
        cx.spawn(async move |this, cx| {
            let mut timed_out = false;
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move {
                    handle
                        .query_if_current(Query::OtherTracksCount, ticket)
                        .await
                },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        this.publish_other_count(&read, response, cx);
                        this.sync_search_caption(cx);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if !timed_out {
                    this.publish_other_count(&read, response, cx);
                }
                this.library.retire_other_count(&read);
                if this.demanded() && (timed_out || !read.ticket.is_current()) {
                    this.request_other_count(cx);
                }
                this.sync_search_caption(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn publish_other_count(
        &mut self,
        read: &model::OtherCountRequest,
        response: rivu_protocol::response::QueryResponse,
        cx: &mut Context<Self>,
    ) {
        if self.library.publish_other_count(
            read,
            &self.state.system.config.library_roots,
            self.state.query_revisions(None),
            &response,
            cx.background_executor().now(),
        ) && let Err(error) = response.result
        {
            self.report_error(error.to_string(), cx);
        }
    }

    pub(super) fn sync_search_caption(&self, cx: &mut Context<Self>) {
        let (matched, total) = self
            .library
            .search_count_parts(cx.background_executor().now());
        self.library_search.update(cx, |search, cx| {
            if !self.library.filtered() && total.is_none() {
                search.clear_counts(cx);
            } else {
                set_search_count_parts(search, matched, total, &self.i18n, cx);
            }
        });
    }

    pub(in crate::gui) fn view_page_size(&self) -> usize {
        self.state.system.config.query_batch_size as usize
    }
    fn request_library_search_counts(&mut self, cx: &mut Context<Self>) {
        let (_, request) = self.library.prepare_search_counts(
            &self.state.system.config.library_roots,
            self.state.query_revisions(None),
            cx.background_executor().now(),
        );
        self.sync_search_caption(cx);
        self.schedule_refresh_wake(cx);
        let Some(request) = request else {
            return;
        };
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let read = &request;
            let mut timed_out = false;
            let counts = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { library_search_counts(&handle, read).await },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.library.publish_search_counts(
                            read,
                            &this.state.system.config.library_roots,
                            this.state.query_revisions(None),
                            Err(CountBlock::Failed),
                        ) && let Err(error) = response.result
                        {
                            this.report_error(error.to_string(), cx);
                        }
                        this.schedule_refresh_wake(cx);
                        this.sync_search_caption(cx);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refresh(cx);
                if timed_out {
                    this.library.retire_search_counts(&request);
                    if this.demanded() {
                        this.request_library_search_counts(cx);
                    }
                    this.schedule_refresh_wake(cx);
                    return;
                }
                if !this.library.accept_search_counts(
                    request,
                    &this.state.system.config.library_roots,
                    this.state.query_revisions(None),
                    counts,
                ) && this.demanded()
                {
                    this.request_library_search_counts(cx);
                }
                this.sync_search_caption(cx);
                this.schedule_refresh_wake(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn request_directory_page(
        &mut self,
        branch: TreeBranch,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let page_size = self.view_page_size();
        if !self.demanded()
            || (!branch.is_forest()
                && self
                    .library
                    .tree()
                    .location_index(&branch, offset / page_size * page_size)
                    .is_none())
        {
            return;
        }
        self.request_library_search_counts(cx);
        let state = Rc::clone(&self.state);
        let request = self.prepare_directory_read(
            branch,
            offset,
            page_size,
            &state.system.config.library_roots,
            state.query_revisions(None),
            cx.background_executor().now(),
        );
        self.schedule_refresh_wake(cx);
        if request.published {
            self.resume_library_navigation(cx);
            cx.notify();
        }
        let Some(read) = request.read else {
            return;
        };
        self.start_loading_text(read.branch.clone(), read.generation, cx);
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            if !this
                .update(cx, |this, cx| {
                    this.refresh(cx);
                    if this.demanded()
                        && this.directory_read_current(&read, this.state.query_revisions(None))
                    {
                        return true;
                    }
                    this.abandon_directory_read(&read);
                    this.finish_loading_text(&read.branch, read.generation, cx);
                    this.dispatch_directory_demand(cx);
                    cx.notify();
                    false
                })
                .unwrap_or(false)
            {
                return;
            }
            let query = read.query.clone();
            let ticket = read.ticket.clone();
            let mut timed_out = false;
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(query, ticket).await },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        this.finish_loading_text(&read.branch, read.generation, cx);
                        if let Some(error) = this.publish_directory_read(
                            &read,
                            this.state.query_revisions(None),
                            response,
                            cx.background_executor().now(),
                        ) {
                            this.report_error(error, cx);
                        }
                        this.schedule_refresh_wake(cx);
                        cx.notify();
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                this.finish_loading_text(&read.branch, read.generation, cx);
                this.refresh(cx);
                if timed_out {
                    this.abandon_directory_read(&read);
                } else if let Some(error) = this.complete_directory_read(
                    &read,
                    this.state.query_revisions(None),
                    response,
                    cx.background_executor().now(),
                ) {
                    this.report_error(error, cx);
                }
                this.resume_library_navigation(cx);
                this.dispatch_directory_demand(cx);
                this.schedule_refresh_wake(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dispatch_directory_demand(&mut self, cx: &mut Context<Self>) {
        let demanded = self.demanded();
        while let Some((branch, offset)) = self.next_directory_demand(demanded) {
            self.request_directory_page(branch, offset, cx);
        }
    }
    pub(in crate::gui) fn toggle_library_view(&mut self, cx: &mut Context<Self>) {
        self.library.toggle_view();
        self.request_directory_page(TreeBranch::default(), 0, cx);
        cx.notify();
    }

    pub(in crate::gui) fn refresh_favorite_filter(&mut self, cx: &mut Context<Self>) {
        self.library.toggle_favorites();
        self.request_directory_page(TreeBranch::default(), 0, cx);
    }

    pub(in crate::gui) fn apply_library_search(
        &mut self,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let query = query.unwrap_or_default().trim().to_lowercase();
        if self.library.buffer().query.as_deref().unwrap_or_default() == query {
            return;
        }
        self.cancel_selection(cx);
        cx.emit(LibraryEvent::SearchChanged);
        self.library.refresh_search(query);
        self.request_directory_page(TreeBranch::default(), 0, cx);
        cx.notify();
    }

    pub(in crate::gui) fn refresh_filter(&mut self, cx: &mut Context<Self>) {
        self.library.refresh_filter(cx.background_executor().now());
        self.request_directory_page(TreeBranch::default(), 0, cx);
    }
}
#[cfg(test)]
mod tests {
    use crate::test_support::ConfigFixture;

    #[test]
    fn library_search_counts_query_resumes_cancelled_reads_and_tracks_scope_changes() {
        use crate::gui::library::model::LibraryBrowser;
        use crate::test_support::CoreRuntime;
        use rivu::test_support::catalog::CatalogFixture;
        use rivu_format::config::Config;
        use rivu_protocol::model::{Command, ConfigMutation, Query};
        use rivu_protocol::response::ViewResponse;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("music");
        let nested = parent.join("album");
        let sibling = directory.path().join("other");
        let roots = vec![parent.clone(), sibling.clone()];
        let store = CatalogFixture::open(&directory.path().join("library.db")).unwrap();
        store
            .seed_tracks(
                directory.path().to_path_buf(),
                [
                    (parent.join("one.wav"), "needle"),
                    (nested.join("two.wav"), "other"),
                    (sibling.join("three.wav"), "needle"),
                    (sibling.join("four.wav"), "other"),
                    (directory.path().join("outside.wav"), "needle"),
                ]
                .into_iter()
                .map(|(path, title)| (path, title.into()))
                .collect(),
            )
            .unwrap();
        drop(store);
        let config_path = directory.path().join("config.toml");
        Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            library_roots: roots.clone(),
            ..Config::default()
        }
        .save_fixture(&config_path)
        .unwrap();
        let runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
        let response = runtime
            .handle
            .query(Query::LibraryRootCounts { roots: Vec::new() });
        assert!(response.result.is_ok());
        let revisions = response.revisions;
        let mut library = LibraryBrowser::default();
        assert!(
            library
                .prepare_search_counts(&roots, revisions, std::time::Instant::now())
                .1
                .is_none()
        );
        let load_other_total =
            |library: &mut LibraryBrowser, roots: &[std::path::PathBuf], revisions| {
                let request = library.prepare_other_count().unwrap();
                let response = runtime.handle.query(Query::OtherTracksCount);
                assert!(matches!(
                    response.result,
                    Ok(ViewResponse::OtherTracksCount(_))
                ));
                assert!(library.publish_other_count(
                    &request,
                    roots,
                    revisions,
                    &response,
                    std::time::Instant::now()
                ));
                library.retire_other_count(&request);
            };
        let load_totals = |library: &mut LibraryBrowser, roots: &[std::path::PathBuf]| {
            let (generation, offsets) =
                library.begin_root_counts(roots, revisions.structure, std::time::Instant::now());
            if let Some(offset) = offsets {
                let response = runtime.handle.query(Query::LibraryRootCounts {
                    roots: roots.to_vec(),
                });
                let Ok(ViewResponse::LibraryRootCounts(counts)) = response.result else {
                    panic!("expected batched root totals");
                };
                library.accept_root_counts(generation, offset, counts);
            }
        };
        library.refresh_search("needle".into());
        let (_, request) =
            library.prepare_search_counts(&roots, revisions, std::time::Instant::now());
        let request = request.unwrap();
        library.cancel_pending_reads();
        let cancelled =
            futures::executor::block_on(super::library_search_counts(&runtime.handle, &request));
        assert!(matches!(cancelled, Err(super::CountBlock::Retired)));
        assert!(!library.accept_search_counts(request, &roots, revisions, cancelled));
        let (_, request) =
            library.prepare_search_counts(&roots, revisions, std::time::Instant::now());
        let request = request.unwrap();
        let result =
            futures::executor::block_on(super::library_search_counts(&runtime.handle, &request))
                .unwrap();
        assert_eq!(result.matched, 3);
        assert_eq!(result.other_matched, 1);
        assert_eq!(result.roots.get(&parent), Some(&1));
        assert_eq!(result.roots.get(&sibling), Some(&1));
        assert!(library.accept_search_counts(request, &roots, revisions, Ok(result)));
        assert_eq!(
            library.search_count_parts(std::time::Instant::now()),
            (Some(3), None)
        );
        assert_eq!(
            library.root_count_label(&parent, std::time::Instant::now()),
            "1 / -"
        );
        load_totals(&mut library, &roots);
        load_other_total(&mut library, &roots, revisions);
        assert_eq!(
            library.search_counts(std::time::Instant::now()),
            Some((3, 5))
        );
        library.refresh_search("not indexed".into());
        let (_, request) =
            library.prepare_search_counts(&roots, revisions, std::time::Instant::now());
        let request = request.unwrap();
        assert_eq!(
            library.search_count_parts(std::time::Instant::now()),
            (None, Some(5))
        );
        assert_eq!(
            library.root_count_label(&parent, std::time::Instant::now()),
            "- / 2"
        );
        assert_eq!(
            library.other_count_label(std::time::Instant::now()),
            "- / 1"
        );
        let result =
            futures::executor::block_on(super::library_search_counts(&runtime.handle, &request))
                .unwrap();
        assert_eq!(result.matched, 0);
        assert_eq!(result.other_matched, 0);
        assert!(library.accept_search_counts(request, &roots, revisions, Ok(result)));
        assert_eq!(
            library.search_counts(std::time::Instant::now()),
            Some((0, 5))
        );
        assert!(!library.other_tracks_empty());
        assert_eq!(library.tree().root_total(), Some(3));
        library.refresh_search("needle".into());
        let pending = library
            .prepare_search_counts(&roots, revisions, std::time::Instant::now())
            .1
            .unwrap();
        for root in &roots {
            // A query on an independent IPC worker is not a command FIFO fence.
            let ack = futures::executor::block_on(runtime.handle.request_command(
                Command::MutateConfig {
                    change: ConfigMutation::RemoveLibraryRoot(root.clone()),
                },
            ));
            assert!(ack.is_success(), "{ack:?}");
        }
        // Core now answers from a newer structure, before the GUI has observed it.
        let obsolete =
            futures::executor::block_on(super::library_search_counts(&runtime.handle, &pending));
        assert!(matches!(obsolete, Err(super::CountBlock::Retired)));
        assert!(library.accept_search_counts(pending, &roots, revisions, obsolete));
        assert!(library.root_counts_retired());
        assert!(!library.root_count_failed(&parent));
        assert!(
            library
                .prepare_search_counts(&roots, revisions, std::time::Instant::now())
                .1
                .is_none()
        );
        let revisions = runtime.handle.query(Query::OtherTracksCount).revisions;
        let (_, request) = library.prepare_search_counts(&[], revisions, std::time::Instant::now());
        let request = request.unwrap();
        let result =
            futures::executor::block_on(super::library_search_counts(&runtime.handle, &request))
                .unwrap();
        assert_eq!(result.matched, 3);
        assert_eq!(result.other_matched, 3);
        assert!(result.roots.is_empty());
        assert!(library.accept_search_counts(request, &[], revisions, Ok(result)));
        load_other_total(&mut library, &[], revisions);
        assert_eq!(
            library.search_counts(std::time::Instant::now()),
            Some((3, 5))
        );
    }
}
