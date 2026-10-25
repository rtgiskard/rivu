//! Library-owned query dispatch; worker retirement is independent of visibility.
use super::{Library, LibraryEvent, TreeBranch, model};
use crate::gui::components::search::set_search_counts;
use gpui::Context;
use rivu_protocol::{model::Query, response::ViewResponse};
use std::rc::Rc;

fn library_search_counts(
    handle: &crate::core::CoreHandle,
    request: &model::SearchCountRequest,
) -> Option<model::SearchCountResult> {
    let model::SearchCountRequest {
        query: worker_query,
        roots,
        revisions,
        ticket,
        ..
    } = request;
    let Query::LibraryRootPage {
        query,
        favorite,
        missing,
        sort,
        ..
    } = worker_query
    else {
        unreachable!()
    };
    let mut matched = 0;
    let mut total = 0;
    let mut root_counts = std::collections::HashMap::new();
    for (index, root) in roots.iter().enumerate() {
        // Nested configured roots describe the same tracks, not extra occurrences.
        if roots.iter().enumerate().any(|(other_index, parent)| {
            index != other_index
                && root.starts_with(parent)
                && (root != parent || other_index < index)
        }) {
            continue;
        }
        let response = handle.query_if_current(
            Query::LibraryRootPage {
                path: root.clone(),
                query: query.clone(),
                favorite: *favorite,
                missing: *missing,
                sort: *sort,
                offset: 0,
                limit: 0,
            },
            ticket.clone(),
        );
        if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
            return None;
        }
        let Ok(ViewResponse::LibraryPage(page)) = response.result else {
            return None;
        };
        let root_matched = page.total;
        let root_total = if query.is_none() && favorite.is_none() && missing.is_none() {
            root_matched
        } else {
            let response = handle.query_if_current(
                Query::LibraryRootPage {
                    path: root.clone(),
                    query: None,
                    favorite: None,
                    missing: None,
                    sort: *sort,
                    offset: 0,
                    limit: 0,
                },
                ticket.clone(),
            );
            if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
                return None;
            }
            let Ok(ViewResponse::LibraryPage(page)) = response.result else {
                return None;
            };
            page.total
        };
        matched += root_matched;
        total += root_total;
        root_counts.insert(root.clone(), (root_matched, root_total));
    }
    let response = handle.query_if_current(
        Query::OtherTracksPage {
            query: query.clone(),
            favorite: *favorite,
            missing: *missing,
            sort: *sort,
            offset: 0,
            limit: 0,
        },
        ticket.clone(),
    );
    if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
        return None;
    }
    let Ok(ViewResponse::LibraryPage(page)) = response.result else {
        return None;
    };
    let other_matched = page.total;
    let other_total = if query.is_none() && favorite.is_none() && missing.is_none() {
        other_matched
    } else {
        let response = handle.query_if_current(Query::OtherTracksCount, ticket.clone());
        if worker_query.stamp(response.revisions) != worker_query.stamp(*revisions) {
            return None;
        }
        let Ok(ViewResponse::OtherTracksCount(total)) = response.result else {
            return None;
        };
        total
    };
    Some(model::SearchCountResult {
        totals: (matched + other_matched, total + other_total),
        roots: root_counts,
        other_tracks: (other_matched, other_total),
    })
}

impl Library {
    pub(in crate::gui) fn request_root_counts(&mut self, cx: &mut Context<Self>) {
        if !self.demanded() {
            return;
        }
        self.request_library_search_counts(cx);
        let state = Rc::clone(&self.state);
        let Some(read) = self.prepare_root_count_read(
            &state.system.config.library_roots,
            state.library.structure_revision,
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
                move |handle| handle.query_if_current(query, ticket),
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
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::gui) fn view_page_size(&self) -> usize {
        self.state.system.config.page_size as usize
    }
    fn request_library_search_counts(&mut self, cx: &mut Context<Self>) {
        let (changed, request) = self.library.prepare_search_counts(
            &self.state.system.config.library_roots,
            self.state.query_revisions(None),
        );
        if changed {
            self.library_search
                .update(cx, |search, cx| search.clear_counts(cx));
        }
        let Some(request) = request else {
            return;
        };
        let handle = self.context.handle().clone();
        cx.spawn(async move |this, cx| {
            let timeout_request = request.clone();
            let mut timed_out = false;
            let (request, counts) = crate::gui::query::run(
                handle,
                cx,
                move |handle| {
                    let counts = library_search_counts(&handle, &request);
                    (request, counts)
                },
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        if this.library.publish_search_counts(
                            &timeout_request,
                            &this.state.system.config.library_roots,
                            this.state.query_revisions(None),
                            None,
                        ) && let Err(error) = response.result
                        {
                            this.report_error(error.to_string(), cx);
                        }
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
                    cx.notify();
                    return;
                }
                if this.library.accept_search_counts(
                    request,
                    &this.state.system.config.library_roots,
                    this.state.query_revisions(None),
                    counts,
                ) {
                    if let Some((matched, total)) = this.library.search_counts() {
                        this.library_search.update(cx, |search, cx| {
                            set_search_counts(search, matched, total, &this.i18n, cx);
                        });
                    }
                } else if this.demanded() {
                    this.request_library_search_counts(cx);
                }
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
        if !self.demanded() {
            return;
        }
        self.request_library_search_counts(cx);
        let state = Rc::clone(&self.state);
        let request = self.prepare_directory_read(
            branch,
            offset,
            self.view_page_size(),
            &state.system.config.library_roots,
            state.query_revisions(None),
        );
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
                move |handle| handle.query_if_current(query, ticket),
                |response, cx| {
                    timed_out = true;
                    let _ = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        this.finish_loading_text(&read.branch, read.generation, cx);
                        if let Some(error) = this.publish_directory_read(
                            &read,
                            this.state.query_revisions(None),
                            response,
                        ) {
                            this.report_error(error, cx);
                        }
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
                } else if let Some(error) =
                    this.complete_directory_read(&read, this.state.query_revisions(None), response)
                {
                    this.report_error(error, cx);
                }
                this.resume_library_navigation(cx);
                this.dispatch_directory_demand(cx);
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
        self.library.refresh_filter();
        self.request_directory_page(TreeBranch::default(), 0, cx);
    }
}
#[cfg(test)]
mod tests {
    use crate::config::ConfigPersistence;

    #[test]
    fn library_search_counts_query_resumes_cancelled_reads_and_tracks_scope_changes() {
        use crate::{
            config::Config,
            core::Runtime,
            gui::library::model::LibraryBrowser,
            library::{ScanRecord, ScanResult},
            store::Store,
        };
        use rivu_audio::MediaInfo;
        use rivu_protocol::model::Query;

        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("music");
        let nested = parent.join("album");
        let sibling = directory.path().join("other");
        let roots = vec![parent.clone(), sibling.clone()];
        let store = Store::open(&directory.path().join("library.db")).unwrap();
        store
            .apply_scan(&ScanResult {
                roots: vec![directory.path().to_path_buf()],
                records: [
                    (parent.join("one.wav"), "needle"),
                    (nested.join("two.wav"), "other"),
                    (sibling.join("three.wav"), "needle"),
                    (sibling.join("four.wav"), "other"),
                    (directory.path().join("outside.wav"), "needle"),
                ]
                .into_iter()
                .map(|(path, title)| ScanRecord {
                    path,
                    size: 1,
                    modified_ns: 0,
                    fingerprint: None,
                    cue: None,
                    media: MediaInfo {
                        title: title.into(),
                        artist: String::new(),
                        album: String::new(),
                        duration: Some(1.0),
                        bitrate_bps: None,
                        bitrate_exact: false,
                        track_number: None,
                        disc_number: None,
                        bits_per_sample: None,
                        release_date: None,
                        codec: "PCM_S16LE".into(),
                        channels: 1,
                        sample_rate: 8000,
                    },
                })
                .collect(),
                ..Default::default()
            })
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
        .save(&config_path)
        .unwrap();
        let runtime = Runtime::start(directory.path(), &config_path).unwrap();
        let response = runtime
            .handle
            .query(Query::LibraryRootCounts { roots: Vec::new() });
        assert!(response.result.is_ok());
        let revisions = response.revisions;
        let mut library = LibraryBrowser::default();
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (5, 5));
        assert_eq!(result.other_tracks, (1, 1));
        assert_eq!(result.roots.len(), 2);
        assert_eq!(result.roots.get(&parent), Some(&(2, 2)));
        assert_eq!(result.roots.get(&sibling), Some(&(2, 2)));
        assert!(library.accept_search_counts(request, &roots, revisions, Some(result)));
        library.refresh_search("needle".into());
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        library.cancel_pending_reads();
        let cancelled = super::library_search_counts(&runtime.handle, &request);
        assert!(cancelled.is_none());
        assert!(!library.accept_search_counts(request, &roots, revisions, cancelled));
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (3, 5));
        assert_eq!(result.other_tracks, (1, 1));
        assert!(library.accept_search_counts(request, &roots, revisions, Some(result)));
        assert_eq!(library.search_counts(), Some((3, 5)));
        assert_eq!(library.root_count_label(&parent), "1 / 2");
        assert_eq!(library.root_count_label(&sibling), "1 / 2");
        library.refresh_search("not indexed".into());
        let (_, request) = library.prepare_search_counts(&roots, revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (0, 5));
        assert_eq!(result.other_tracks, (0, 1));
        assert!(library.accept_search_counts(request, &roots, revisions, Some(result)));
        assert!(!library.other_tracks_empty());
        assert_eq!(library.tree().root_total(), Some(3));
        library.refresh_search("needle".into());
        for root in &roots {
            runtime
                .handle
                .update_gui_config(crate::core::GuiConfigChange::RemoveLibraryRoot(
                    root.clone(),
                ))
                .unwrap();
        }
        let revisions = runtime.handle.query(Query::OtherTracksCount).revisions;
        let (_, request) = library.prepare_search_counts(&[], revisions);
        let request = request.unwrap();
        let result = super::library_search_counts(&runtime.handle, &request).unwrap();
        assert_eq!(result.totals, (3, 5));
        assert_eq!(result.other_tracks, (3, 5));
        assert!(result.roots.is_empty());
        assert!(library.accept_search_counts(request, &[], revisions, Some(result)));
        assert_eq!(library.search_counts(), Some((3, 5)));
    }
}
