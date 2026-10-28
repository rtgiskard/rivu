//! Library logical demand and physical worker gates have separate lifetimes.
use super::Library;
#[cfg(test)]
use crate::config::ConfigPersistence;
use crate::gui::library;
use rivu_protocol::{
    model::Query,
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const DIRECTORY_WORKERS: usize = 4;
#[derive(Default)]
pub(super) struct LibraryRequests {
    generation: u64,
    directory_workers: HashMap<library::TreeBranch, (usize, u64)>,
    directory_queued: VecDeque<(library::TreeBranch, usize)>,
    root_count_workers: HashSet<usize>,
}
impl LibraryRequests {
    pub(in crate::gui) fn cancel_directory_demand(&mut self) {
        self.directory_queued.clear();
    }

    pub(in crate::gui) fn queue_directory(&mut self, branch: library::TreeBranch, offset: usize) {
        if let Some(index) = self
            .directory_queued
            .iter()
            .position(|(key, _)| key == &branch)
        {
            self.directory_queued.remove(index);
        } else if self.directory_queued.len() == DIRECTORY_WORKERS {
            self.directory_queued.pop_front();
        }
        // Unsubmitted demand owns no pending token; a still-visible dropped row
        // can demand again after any completion, without retaining a visited-tree log.
        self.directory_queued.push_back((branch, offset));
    }

    pub(in crate::gui) fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub(super) fn reads_pending(&self) -> bool {
        !self.directory_workers.is_empty() || !self.root_count_workers.is_empty()
    }
}
/// Local publication needs a redraw; queued demand alone must wait for a worker.
pub(in crate::gui) struct DirectoryRequest {
    pub(in crate::gui) published: bool,
    pub(in crate::gui) read: Option<DirectoryRead>,
}

/// A physical directory read; cancellation never releases its worker reservation.
pub(in crate::gui) struct DirectoryRead {
    pub(in crate::gui) branch: library::TreeBranch,
    pub(in crate::gui) generation: u64,
    pub(in crate::gui) query: Query,
    pub(in crate::gui) ticket: crate::core::QueryTicket,
    owner_query: Query,
    owner_generation: u64,
}

pub(in crate::gui) struct RootCountRead {
    pub(in crate::gui) query: Query,
    pub(in crate::gui) ticket: crate::core::QueryTicket,
    generation: u64,
    offset: usize,
}

impl Library {
    pub(in crate::gui) fn prepare_directory_read(
        &mut self,
        branch: library::TreeBranch,
        offset: usize,
        page_size: usize,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
    ) -> DirectoryRequest {
        if self.library.sync_projection(revisions) {
            self.requests.cancel_directory_demand();
        }
        let offset = offset / page_size * page_size;
        if self.requests.directory_workers.get(&branch).is_some_and(
            |(worker_offset, generation)| {
                *worker_offset == offset
                    && self.library.tree().request_pending(&branch, *generation)
            },
        ) {
            self.requests
                .directory_queued
                .retain(|(queued, _)| queued != &branch);
            return DirectoryRequest {
                published: false,
                read: None,
            };
        }
        if !branch.is_forest()
            && (self.requests.directory_workers.contains_key(&branch)
                || self.requests.directory_workers.len() >= DIRECTORY_WORKERS)
        {
            self.requests.queue_directory(branch, offset);
            return DirectoryRequest {
                published: false,
                read: None,
            };
        }
        self.requests
            .directory_queued
            .retain(|(queued, _)| queued != &branch);
        let generation = self.requests.next_generation();
        let offset = self
            .library
            .prepare_directory(branch.clone(), offset, page_size, generation);
        if branch.is_forest() {
            let page = self.library.forest_page(roots, offset, page_size);
            self.library.accept_directory(&branch, generation, page);
            return DirectoryRequest {
                published: true,
                read: None,
            };
        }
        let buffer = self.library.buffer();
        let query = if branch.is_other_tracks() {
            Query::OtherTracksPage {
                query: buffer.query.clone(),
                favorite: buffer.favorite,
                missing: buffer.missing,
                sort: buffer.sort,
                offset,
                limit: page_size,
            }
        } else if self.library.tree_active() {
            Query::DirectoryPage {
                path: branch.path().to_path_buf(),
                query: buffer.query.clone(),
                favorite: buffer.favorite,
                missing: buffer.missing,
                offset,
                limit: page_size,
            }
        } else {
            Query::LibraryRootPage {
                path: branch.path().to_path_buf(),
                query: buffer.query.clone(),
                favorite: buffer.favorite,
                missing: buffer.missing,
                sort: buffer.sort,
                offset,
                limit: page_size,
            }
        };
        self.requests
            .directory_workers
            .insert(branch.clone(), (offset, generation));
        DirectoryRequest {
            published: false,
            read: Some(DirectoryRead {
                branch,
                generation,
                query,
                owner_query: self.library.projection_query(),
                owner_generation: self.library.projection.generation(),
                ticket: self.library.projection.ticket(),
            }),
        }
    }

    pub(in crate::gui) fn directory_read_current(
        &self,
        read: &DirectoryRead,
        revisions: QueryRevisions,
    ) -> bool {
        self.library
            .tree()
            .request_pending(&read.branch, read.generation)
            && self.library.projection_query() == read.owner_query
            && self
                .library
                .projection
                .accepts(&read.owner_query, read.owner_generation, revisions)
    }

    pub(in crate::gui) fn abandon_directory_read(&mut self, read: &DirectoryRead) {
        self.requests.directory_workers.remove(&read.branch);
        self.library.cancel_directory(&read.branch, read.generation);
    }

    pub(in crate::gui) fn complete_directory_read(
        &mut self,
        read: &DirectoryRead,
        revisions: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        self.requests.directory_workers.remove(&read.branch);
        self.publish_directory_read(read, revisions, response)
    }

    pub(super) fn publish_directory_read(
        &mut self,
        read: &DirectoryRead,
        revisions: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        if !self.directory_read_current(read, revisions)
            || !self.library.projection.accepts(
                &read.owner_query,
                read.owner_generation,
                response.revisions,
            )
        {
            self.library.cancel_directory(&read.branch, read.generation);
            return None;
        }
        let page = match response.result {
            Ok(ViewResponse::DirectoryPage(page))
                if matches!(read.query, Query::DirectoryPage { .. }) =>
            {
                page
            }
            Ok(ViewResponse::LibraryPage(page))
                if matches!(
                    read.query,
                    Query::LibraryRootPage { .. } | Query::OtherTracksPage { .. }
                ) =>
            {
                rivu_protocol::model::Page {
                    total: page.total,
                    rows: page
                        .rows
                        .into_iter()
                        .map(rivu_protocol::model::DirectoryRow::Track)
                        .collect(),
                }
            }
            Err(error) => {
                self.library.fail_directory(&read.branch, read.generation);
                return Some(error.to_string());
            }
            _ => {
                self.library.fail_directory(&read.branch, read.generation);
                return None;
            }
        };
        self.library
            .accept_directory(&read.branch, read.generation, page);
        None
    }

    pub(super) fn prune_directory_demand(&mut self) {
        self.requests.directory_queued.retain(|(branch, offset)| {
            branch.is_forest()
                || self
                    .library
                    .tree()
                    .location_index(branch, *offset)
                    .is_some()
        });
    }

    pub(in crate::gui) fn next_directory_demand(
        &mut self,
        demanded: bool,
    ) -> Option<(library::TreeBranch, usize)> {
        if !demanded {
            self.requests.cancel_directory_demand();
            return None;
        }
        self.prune_directory_demand();
        if self.requests.directory_workers.len() >= DIRECTORY_WORKERS {
            return None;
        }
        let index = self
            .requests
            .directory_queued
            .iter()
            .position(|(branch, _)| !self.requests.directory_workers.contains_key(branch))?;
        self.requests.directory_queued.remove(index)
    }

    pub(in crate::gui) fn prepare_root_count_read(
        &mut self,
        roots: &[std::path::PathBuf],
        structure: u64,
    ) -> Option<RootCountRead> {
        self.library.sync_root_counts(roots, structure);
        if !self.requests.root_count_workers.is_empty() || self.library.root_counts_settled() {
            return None;
        }
        let (generation, batches) = self.library.begin_root_counts(roots, structure);
        let offset = batches.into_iter().next()?;
        self.requests.root_count_workers.insert(offset);
        Some(RootCountRead {
            query: Query::LibraryRootCounts {
                roots: roots
                    [offset..(offset + rivu_protocol::model::MAX_QUERY_ROWS).min(roots.len())]
                    .to_vec(),
            },
            ticket: self.library.root_count_ticket(),
            generation,
            offset,
        })
    }

    pub(in crate::gui) fn complete_root_count_read(
        &mut self,
        read: RootCountRead,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        self.requests.root_count_workers.remove(&read.offset);
        self.publish_root_count_read(&read, roots, revisions, response)
    }

    pub(super) fn retire_root_count_read(&mut self, read: &RootCountRead) {
        self.requests.root_count_workers.remove(&read.offset);
    }

    pub(super) fn publish_root_count_read(
        &mut self,
        read: &RootCountRead,
        roots: &[std::path::PathBuf],
        revisions: QueryRevisions,
        response: QueryResponse,
    ) -> Option<String> {
        if !self
            .library
            .counts_current(read.generation, roots, revisions.structure)
        {
            return None;
        }
        if read.query.stamp(response.revisions) != read.query.stamp(revisions) {
            self.library.fail_root_counts(read.generation, read.offset);
            return None;
        }
        match response.result {
            Ok(ViewResponse::LibraryRootCounts(counts)) => {
                self.library
                    .accept_root_counts(read.generation, read.offset, counts)
            }
            Err(error) => {
                self.library.fail_root_counts(read.generation, read.offset);
                return Some(error.to_string());
            }
            _ => self.library.fail_root_counts(read.generation, read.offset),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    fn with_library(
        cx: &mut gpui::TestAppContext,
        run: impl FnOnce(&mut Library, &mut gpui::Context<Library>),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        crate::config::Config {
            volume: 0.0,
            ffmpeg_enabled: false,
            mpris_enabled: false,
            ..Default::default()
        }
        .save(&config_path)
        .unwrap();
        let runtime = crate::core::Runtime::start(directory.path(), &config_path).unwrap();
        let i18n = std::sync::Arc::new(crate::i18n::Localizer::new("en-US"));
        let labels = std::rc::Rc::new(
            i18n.fixed_texts()
                .map(|(id, text)| (id.to_owned(), gpui::SharedString::from(text.to_owned())))
                .collect(),
        );
        let context = crate::gui::feature::FeatureContext {
            snapshots: std::rc::Rc::new(crate::gui::feature::SnapshotSource::new(
                runtime.handle.clone(),
            )),
            i18n,
            labels,
            scroll: Default::default(),
        };
        let owner = cx.new(|cx| Library::new(context, cx));
        owner.update(cx, |owner, cx| run(owner, cx));
    }

    #[gpui::test]
    fn other_collection_is_present_while_total_unknown_and_uses_its_own_query(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, _| {
            let revisions = QueryRevisions::default();
            let forest = browser.prepare_directory_read(
                library::TreeBranch::default(),
                0,
                20,
                &[],
                revisions,
            );
            assert!(forest.published);
            assert!(forest.read.is_none());
            assert_eq!(browser.library.tree().root_total(), Some(1));
            let Some(library::tree::TreeItem::Entry(entry)) = browser.library.tree().item(0) else {
                panic!("other collection row");
            };
            assert!(matches!(
                entry.row,
                rivu_protocol::model::DirectoryRow::OtherTracks
            ));
            let branch = entry.branch.expect("explicit collection branch");
            assert!(branch.is_other_tracks());
            assert!(!branch.is_forest());
            browser.library.expand_directory(&branch, true);
            browser.library.buffer.query = Some("needle".into());
            browser.library.buffer.favorite = Some(true);
            browser.library.buffer.missing = Some(true);
            let read = browser
                .prepare_directory_read(branch, 0, 20, &[], revisions)
                .read
                .expect("collection read");
            assert!(
                matches!(&read.query, Query::OtherTracksPage { query: Some(query), favorite: Some(true), missing: Some(true), offset: 0, limit: 20, .. } if query == "needle")
            );
        });
    }

    #[gpui::test]
    fn forest_pages_keep_configured_root_order_and_append_other(cx: &mut gpui::TestAppContext) {
        with_library(cx, |browser, _| {
            let roots = vec!["/b".into(), "/a".into(), "/b/child".into()];
            let forest = library::TreeBranch::default();
            for offset in [0, 2] {
                let request = browser.prepare_directory_read(
                    forest.clone(),
                    offset,
                    2,
                    &roots,
                    QueryRevisions::default(),
                );
                assert!(request.published);
                assert!(request.read.is_none());
            }
            assert_eq!(browser.library.tree().root_total(), Some(4));
            for (index, root) in roots.iter().enumerate() {
                assert!(matches!(
                    browser.library.tree().cached_row(&forest, index),
                    Some(rivu_protocol::model::DirectoryRow::Directory { path }) if path == root
                ));
            }
            assert!(matches!(
                browser.library.tree().cached_row(&forest, roots.len()),
                Some(rivu_protocol::model::DirectoryRow::OtherTracks)
            ));
        });
    }

    fn root_branches(
        browser: &mut Library,
        roots: &[std::path::PathBuf],
    ) -> Vec<library::TreeBranch> {
        let request = browser.prepare_directory_read(
            library::TreeBranch::default(),
            0,
            20,
            roots,
            QueryRevisions::default(),
        );
        assert!(request.published);
        assert!(request.read.is_none());
        (0..roots.len())
            .map(|index| {
                let library::tree::TreeItem::Entry(entry) =
                    browser.library.tree().item(index).unwrap()
                else {
                    panic!("root entry");
                };
                entry.branch.unwrap()
            })
            .collect()
    }

    fn track_response(revisions: QueryRevisions, flat: bool) -> QueryResponse {
        let track = rivu_protocol::model::LibraryRow {
            id: 7,
            path: "/music/track.flac".into(),
            title: "Track".into(),
            artist: String::new(),
            album: String::new(),
            duration: None,
            codec: "flac".into(),
            bitrate_bps: None,
            bitrate_exact: false,
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: None,
            cue_number: None,
            favorite: false,
            missing: false,
            play_count: 0,
        };
        QueryResponse {
            revisions,
            result: Ok(if flat {
                ViewResponse::LibraryPage(rivu_protocol::model::Page {
                    total: 1,
                    rows: vec![track],
                })
            } else {
                ViewResponse::DirectoryPage(rivu_protocol::model::Page {
                    total: 1,
                    rows: vec![rivu_protocol::model::DirectoryRow::Track(track)],
                })
            }),
        }
    }

    #[gpui::test]
    fn directory_and_flat_completions_reject_stale_bitrate(cx: &mut gpui::TestAppContext) {
        with_library(cx, |browser, _| {
            for flat in [false, true] {
                if flat {
                    browser.library.toggle_view();
                }
                let roots = vec!["/music".into()];
                let branch = root_branches(browser, &roots).remove(0);
                let baseline = browser.state.query_revisions(None);
                let read = browser
                    .prepare_directory_read(branch.clone(), 0, 20, &roots, baseline)
                    .read
                    .unwrap();
                let state = std::rc::Rc::make_mut(&mut browser.state);
                state.library.bitrate_revision += 1;
                let latest = browser.state.query_revisions(None);
                assert!(browser.library.sync_projection(latest));
                assert!(
                    browser
                        .complete_directory_read(&read, latest, track_response(baseline, flat))
                        .is_none()
                );
                assert!(browser.library.tree().cached_row(&branch, 0).is_none());
                assert!(browser.requests.directory_workers.is_empty());
                assert!(browser.next_directory_demand(true).is_none());
            }
        });
    }

    #[gpui::test]
    fn bitrate_change_retries_only_after_physical_worker_retires(cx: &mut gpui::TestAppContext) {
        with_library(cx, |browser, _| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
            browser.library.expand_directory(&branch, true);
            let baseline = browser.state.query_revisions(None);
            let read = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, baseline)
                .read
                .unwrap();
            let state = std::rc::Rc::make_mut(&mut browser.state);
            state.library.bitrate_revision += 1;
            let latest = browser.state.query_revisions(None);
            assert!(
                browser
                    .publish_directory_read(&read, latest, track_response(baseline, false))
                    .is_none()
            );
            assert!(
                !browser
                    .library
                    .tree()
                    .request_pending(&branch, read.generation)
            );
            assert!(browser.library.tree().cached_row(&branch, 0).is_none());
            assert!(browser.requests.directory_workers.contains_key(&branch));
            assert!(
                browser
                    .prepare_directory_read(branch.clone(), 0, 20, &roots, latest)
                    .read
                    .is_none()
            );
            assert_eq!(browser.requests.directory_queued.len(), 1);
            browser.abandon_directory_read(&read);
            let (retry_branch, offset) = browser.next_directory_demand(true).unwrap();
            assert_eq!(retry_branch, branch);
            let retry = browser
                .prepare_directory_read(retry_branch, offset, 20, &roots, latest)
                .read
                .unwrap();
            assert!(
                browser
                    .complete_directory_read(&retry, latest, track_response(latest, false))
                    .is_none()
            );
            assert!(browser.library.tree().cached_row(&branch, 0).is_some());
            assert!(browser.requests.directory_workers.is_empty());
            assert!(browser.next_directory_demand(true).is_none());
        });
    }
    #[gpui::test]
    fn snapshot_bitrate_reconciliation_preserves_counts_selection_and_geometry(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, cx| {
            let roots = vec![std::path::PathBuf::from("/music")];
            let mut state = (*browser.state).clone();
            let config = std::sync::Arc::make_mut(&mut state.system.config);
            config.library_roots = roots.clone();
            config.page_size = 20;
            browser.visible = true;
            browser.reconcile(std::rc::Rc::new(state), cx);
            let branch = root_branches(browser, &roots).remove(0);
            browser.library.expand_directory(&branch, true);
            let baseline = browser.state.query_revisions(None);
            let count = browser
                .prepare_root_count_read(&roots, baseline.structure)
                .unwrap();
            browser.complete_root_count_read(
                count,
                &roots,
                baseline,
                QueryResponse {
                    revisions: baseline,
                    result: Ok(ViewResponse::LibraryRootCounts(vec![
                        rivu_protocol::model::LibraryRootCount {
                            path: roots[0].clone(),
                            total: 1,
                        },
                    ])),
                },
            );
            let read = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, baseline)
                .read
                .unwrap();
            browser.complete_directory_read(&read, baseline, track_response(baseline, false));
            browser.library.selected.insert(7);
            let geometry = browser.library.tree().row_count();
            let projection = browser.library.tree().projection_revision();
            let mut next = (*browser.state).clone();
            next.library.bitrate_revision += 1;
            browser.reconcile(std::rc::Rc::new(next), cx);
            assert!(browser.library.tree().cached_row(&branch, 0).is_none());
            assert_eq!(browser.library.root_count(&roots[0]), Some(1));
            assert!(
                browser
                    .prepare_root_count_read(&roots, baseline.structure)
                    .is_none()
            );
            assert!(browser.library.selected.contains(&7));
            assert_eq!(browser.library.tree().row_count(), geometry);
            assert_eq!(browser.library.tree().projection_revision(), projection);
            assert_eq!(
                browser.library.tree.requests_for_range(0..geometry),
                vec![(branch, 0)]
            );
        });
    }

    #[gpui::test]
    fn source_revision_changes_reject_old_completions(cx: &mut gpui::TestAppContext) {
        with_library(cx, |browser, _| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
            let baseline = browser.state.query_revisions(None);
            let read = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, baseline)
                .read
                .unwrap();
            let state = std::rc::Rc::make_mut(&mut browser.state);
            state.library.revision += 1;
            let latest = browser.state.query_revisions(None);
            assert!(browser.library.sync_projection(latest));
            assert!(
                browser
                    .complete_directory_read(&read, latest, track_response(baseline, false))
                    .is_none()
            );
            assert!(browser.library.tree().cached_row(&branch, 0).is_none());
            assert!(browser.requests.directory_workers.is_empty());
            assert!(browser.next_directory_demand(true).is_none());
        });
    }

    #[gpui::test]
    fn directory_filters_reject_stale_rows_without_releasing_physical_workers(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, _| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
            browser.library.expand_directory(&branch, true);
            let revisions = QueryRevisions::default();
            let old = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                .read
                .unwrap();
            for filter in ["one", "two", "latest"] {
                browser.library.refresh_search(filter.into());
                assert!(
                    browser
                        .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                        .read
                        .is_none()
                );
                assert_eq!(browser.requests.directory_workers.len(), 1);
                assert!(!browser.directory_read_current(&old, revisions));
            }
            browser.complete_directory_read(
                &old,
                revisions,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::DirectoryPage(rivu_protocol::model::Page {
                        total: 1,
                        rows: vec![rivu_protocol::model::DirectoryRow::Directory {
                            path: "/stale".into(),
                        }],
                    })),
                },
            );
            let (branch, offset) = browser.next_directory_demand(true).unwrap();
            let latest = browser
                .prepare_directory_read(branch, offset, 20, &roots, revisions)
                .read
                .unwrap();
            assert!(
                matches!(&latest.query, Query::DirectoryPage { query: Some(query), .. } if query == "latest")
            );
            assert!(browser.directory_read_current(&latest, revisions));
            browser.complete_directory_read(
                &latest,
                revisions,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::DirectoryPage(rivu_protocol::model::Page {
                        total: 0,
                        rows: vec![],
                    })),
                },
            );
            assert!(browser.requests.directory_workers.is_empty());
            assert!(
                !browser
                    .library
                    .tree()
                    .request_pending(&latest.branch, latest.generation)
            );
        });
    }
    #[gpui::test]
    fn cancelled_directory_and_count_reads_hold_their_worker_gates_until_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, _| {
            let roots: Vec<_> = (0..5)
                .map(|index| std::path::PathBuf::from(format!("/music/{index}")))
                .collect();
            let branches = root_branches(browser, &roots);
            for branch in &branches {
                browser.library.expand_directory(branch, true);
            }
            let revisions = QueryRevisions::default();
            let reads: Vec<_> = branches
                .iter()
                .take(DIRECTORY_WORKERS)
                .map(|branch| {
                    browser
                        .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                        .read
                        .unwrap()
                })
                .collect();
            let count = browser.prepare_root_count_read(&roots, 0).unwrap();
            for _ in 0..8 {
                browser.cancel_reads();
                let queued =
                    browser.prepare_directory_read(branches[4].clone(), 0, 20, &roots, revisions);
                assert!(!queued.published);
                assert!(queued.read.is_none());
                assert_eq!(
                    browser.library.tree().status(&branches[4]),
                    library::BranchStatus::Unloaded
                );
                assert_eq!(browser.requests.directory_queued.len(), 1);
                assert!(browser.prepare_root_count_read(&roots, 0).is_none());
                assert_eq!(browser.requests.directory_workers.len(), DIRECTORY_WORKERS);
                assert_eq!(browser.requests.root_count_workers.len(), 1);
            }
            browser.abandon_directory_read(&reads[0]);
            let (branch, offset) = browser.next_directory_demand(true).unwrap();
            let resumed = browser.prepare_directory_read(branch, offset, 20, &roots, revisions);
            assert!(!resumed.published);
            let resumed = resumed.read.expect("worker slot released");
            assert_eq!(
                browser.library.tree().status(&resumed.branch),
                library::BranchStatus::Loading
            );
            assert_eq!(browser.requests.directory_workers.len(), DIRECTORY_WORKERS);
            browser.complete_root_count_read(
                count,
                &roots,
                revisions,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::LibraryRootCounts(vec![])),
                },
            );
            assert!(browser.prepare_root_count_read(&roots, 0).is_some());
        });
    }

    #[gpui::test]
    fn current_directory_page_demand_does_not_queue_a_duplicate(cx: &mut gpui::TestAppContext) {
        with_library(cx, |browser, _| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
            browser.library.expand_directory(&branch, true);
            let revisions = QueryRevisions::default();
            let read = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                .read
                .unwrap();
            assert!(
                browser
                    .prepare_directory_read(branch.clone(), 19, 20, &roots, revisions)
                    .read
                    .is_none()
            );
            assert!(browser.requests.directory_queued.is_empty());
            browser.prepare_directory_read(branch.clone(), 40, 20, &roots, revisions);
            assert_eq!(
                browser.requests.directory_queued.front(),
                Some(&(branch.clone(), 40))
            );
            browser.prepare_directory_read(branch.clone(), 7, 20, &roots, revisions);
            assert!(browser.requests.directory_queued.is_empty());
            browser.complete_directory_read(&read, revisions, track_response(revisions, false));
            assert!(browser.requests.directory_workers.is_empty());
            assert!(browser.next_directory_demand(true).is_none());
            assert!(browser.library.tree().cached_row(&branch, 0).is_some());
        });
    }

    #[gpui::test]
    fn timeout_holds_worker_until_retirement_and_only_explicit_retry_requeues(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, _| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
            browser.library.expand_directory(&branch, true);
            let revisions = QueryRevisions::default();
            let read = browser
                .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                .read
                .unwrap();
            browser.prepare_directory_read(branch.clone(), 0, 20, &roots, revisions);
            assert_eq!(
                browser.publish_directory_read(
                    &read,
                    revisions,
                    QueryResponse {
                        revisions,
                        result: Err(rivu_protocol::response::QueryError::Message(
                            "timeout".into()
                        )),
                    },
                ),
                Some("timeout".into())
            );
            assert_eq!(
                browser.library.tree().status(&branch),
                library::BranchStatus::Error
            );
            assert!(browser.requests.directory_workers.contains_key(&branch));
            assert!(browser.next_directory_demand(true).is_none());
            browser.library.retry_directory(&branch);
            assert!(
                browser
                    .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                    .read
                    .is_none()
            );
            assert_eq!(browser.requests.directory_queued.len(), 1);
            browser.abandon_directory_read(&read);
            let (retry_branch, offset) = browser.next_directory_demand(true).unwrap();
            let retry = browser
                .prepare_directory_read(retry_branch, offset, 20, &roots, revisions)
                .read
                .unwrap();
            browser.publish_directory_read(&read, revisions, track_response(revisions, false));
            assert!(
                browser
                    .library
                    .tree()
                    .request_pending(&branch, retry.generation)
            );
            browser.complete_directory_read(&retry, revisions, track_response(revisions, false));
            assert!(browser.requests.directory_workers.is_empty());
            assert_eq!(
                browser.library.tree().status(&branch),
                library::BranchStatus::Ready
            );
        });
    }

    #[gpui::test]
    fn collapse_discards_queued_descendants_and_rejects_late_callbacks(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, cx| {
            let roots = vec![
                "/music".into(),
                "/busy-1".into(),
                "/busy-2".into(),
                "/busy-3".into(),
                "/busy-4".into(),
            ];
            let branches = root_branches(browser, &roots);
            let root = &branches[0];
            browser.library.expand_directory(root, true);
            let revisions = QueryRevisions::default();
            let read = browser
                .prepare_directory_read(root.clone(), 0, 20, &roots, revisions)
                .read
                .unwrap();
            browser.complete_directory_read(
                &read,
                revisions,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::DirectoryPage(rivu_protocol::model::Page {
                        total: 1,
                        rows: vec![rivu_protocol::model::DirectoryRow::Directory {
                            path: "/music/album".into(),
                        }],
                    })),
                },
            );
            let library::tree::TreeItem::Entry(entry) = browser.library.tree().item(1).unwrap()
            else {
                panic!("album directory");
            };
            let child = entry.branch.unwrap();
            browser.library.expand_directory(&child, true);
            let reads: Vec<_> = branches
                .iter()
                .skip(1)
                .map(|branch| {
                    browser.library.expand_directory(branch, true);
                    browser
                        .prepare_directory_read(branch.clone(), 0, 20, &roots, revisions)
                        .read
                        .unwrap()
                })
                .collect();
            assert!(
                browser
                    .prepare_directory_read(child.clone(), 7, 20, &roots, revisions)
                    .read
                    .is_none()
            );
            assert_eq!(
                browser.requests.directory_queued.front(),
                Some(&(child.clone(), 0))
            );
            browser.library.expand_directory(root, false);
            browser.prune_directory_demand();
            assert!(browser.requests.directory_queued.is_empty());
            assert!(browser.library.tree().expanded(&child));
            browser.visible = true;
            browser.request_directory_page(child.clone(), 0, cx);
            assert_eq!(browser.requests.directory_workers.len(), DIRECTORY_WORKERS);
            assert!(browser.requests.directory_queued.is_empty());
            browser.abandon_directory_read(&reads[0]);
            assert!(browser.next_directory_demand(true).is_none());
            browser.library.expand_directory(root, true);
            let count = browser.library.tree().row_count();
            assert!(
                browser
                    .library
                    .demand_directories(0..count)
                    .contains(&(child.clone(), 0))
            );
            let reopened = browser
                .prepare_directory_read(child.clone(), 0, 20, &roots, revisions)
                .read
                .expect("current visible demand can read after reopening");
            browser.library.expand_directory(root, false);
            browser.prune_directory_demand();
            browser.complete_directory_read(&reopened, revisions, track_response(revisions, false));
            assert!(browser.library.tree().cached_row(&child, 0).is_none());
            assert!(
                !browser
                    .library
                    .tree()
                    .request_pending(&child, reopened.generation)
            );
            assert!(browser.library.tree().expanded(&child));
            for read in reads.iter().skip(1) {
                browser.abandon_directory_read(read);
            }
            assert!(browser.requests.directory_workers.is_empty());
        });
    }

    #[gpui::test]
    fn cancellation_releases_bounded_queue_even_without_physical_workers(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser, _| {
            let roots: Vec<_> = (0..12)
                .map(|index| format!("/music/{index}").into())
                .collect();
            let branches = root_branches(browser, &roots);
            for branch in branches {
                browser.requests.queue_directory(branch, 0);
                assert!(browser.requests.directory_queued.len() <= DIRECTORY_WORKERS);
            }
            assert_eq!(browser.requests.directory_queued.len(), DIRECTORY_WORKERS);
            assert!(!browser.requests.reads_pending());
            browser.cancel_reads();
            assert!(browser.requests.directory_queued.is_empty());
        });
    }

    #[test]
    fn queued_directory_demand_coalesces_without_releasing_a_worker() {
        let mut requests = LibraryRequests::default();
        let branch = super::library::TreeBranch::default();
        requests.directory_workers.insert(branch.clone(), (0, 1));
        requests.queue_directory(branch.clone(), 0);
        requests.queue_directory(branch.clone(), 40);
        assert_eq!(requests.directory_queued.len(), 1);
        assert_eq!(
            requests.directory_queued.front(),
            Some(&(branch.clone(), 40))
        );
        requests.cancel_directory_demand();
        assert!(requests.directory_queued.is_empty());
        assert!(requests.directory_workers.contains_key(&branch));
    }
}
