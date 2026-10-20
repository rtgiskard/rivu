//! Library logical demand and physical worker gates have separate lifetimes.
use super::Library;
#[cfg(test)]
use crate::config::ConfigPersistence;
use crate::gui::library;
use crate::{
    model::Query,
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use std::collections::{HashSet, VecDeque};

pub(super) const DIRECTORY_WORKERS: usize = 4;
#[derive(Default)]
pub(super) struct LibraryRequests {
    generation: u64,
    directory_workers: HashSet<library::TreeBranch>,
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
        if !branch.is_forest()
            && (self.requests.directory_workers.contains(&branch)
                || self.requests.directory_workers.len() >= DIRECTORY_WORKERS)
        {
            self.requests.queue_directory(branch, offset);
            return DirectoryRequest {
                published: false,
                read: None,
            };
        }
        let generation = self.requests.next_generation();
        let offset = self
            .library
            .prepare_directory(branch.clone(), offset, page_size, generation);
        if branch.is_forest() {
            self.library.accept_directory(
                &branch,
                generation,
                crate::model::Page {
                    total: roots.len(),
                    rows: roots
                        .iter()
                        .skip(offset)
                        .take(page_size)
                        .map(|path| crate::model::DirectoryRow::Directory { path: path.clone() })
                        .collect(),
                },
            );
            return DirectoryRequest {
                published: true,
                read: None,
            };
        }
        let buffer = self.library.buffer();
        let query = if self.library.tree_active() {
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
        self.requests.directory_workers.insert(branch.clone());
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
                if matches!(read.query, Query::LibraryRootPage { .. }) =>
            {
                crate::model::Page {
                    total: page.total,
                    rows: page
                        .rows
                        .into_iter()
                        .map(crate::model::DirectoryRow::Track)
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

    pub(in crate::gui) fn next_directory_demand(
        &mut self,
        demanded: bool,
    ) -> Option<(library::TreeBranch, usize)> {
        if !demanded {
            self.requests.cancel_directory_demand();
            return None;
        }
        if self.requests.directory_workers.len() >= DIRECTORY_WORKERS {
            return None;
        }
        let index = self
            .requests
            .directory_queued
            .iter()
            .position(|(branch, _)| !self.requests.directory_workers.contains(branch))?;
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
                roots: roots[offset..(offset + crate::model::MAX_QUERY_ROWS).min(roots.len())]
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

    fn with_library(cx: &mut gpui::TestAppContext, run: impl FnOnce(&mut Library)) {
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
        owner.update(cx, |owner, _| run(owner));
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
    #[gpui::test]
    fn directory_filters_reject_stale_rows_without_releasing_physical_workers(
        cx: &mut gpui::TestAppContext,
    ) {
        with_library(cx, |browser| {
            let roots = vec!["/music".into()];
            let branch = root_branches(browser, &roots).remove(0);
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
                    result: Ok(ViewResponse::DirectoryPage(crate::model::Page {
                        total: 1,
                        rows: vec![crate::model::DirectoryRow::Directory {
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
                    result: Ok(ViewResponse::DirectoryPage(crate::model::Page {
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
        with_library(cx, |browser| {
            let roots: Vec<_> = (0..5)
                .map(|index| std::path::PathBuf::from(format!("/music/{index}")))
                .collect();
            let branches = root_branches(browser, &roots);
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
    #[test]
    fn queued_directory_demand_coalesces_without_releasing_a_worker() {
        let mut requests = LibraryRequests::default();
        let branch = super::library::TreeBranch::default();
        requests.directory_workers.insert(branch.clone());
        requests.queue_directory(branch.clone(), 0);
        requests.queue_directory(branch.clone(), 40);
        assert_eq!(requests.directory_queued.len(), 1);
        assert_eq!(
            requests.directory_queued.front(),
            Some(&(branch.clone(), 40))
        );
        requests.cancel_directory_demand();
        assert!(requests.directory_queued.is_empty());
        assert!(requests.directory_workers.contains(&branch));
    }
}
