use super::Settings;
use crate::query_tracking::{QueryTicket, QueryTracker};
use gpui::{Context, SharedString};
use rivu_protocol::{
    model::{LibraryRootStats, MAX_QUERY_ROWS, Query},
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(super) struct RootStats {
    owner: QueryTracker,
    roots: Vec<PathBuf>,
    revision: u64,
    values: HashMap<PathBuf, LibraryRootStats>,
    next: usize,
    // A canceled or timed-out query still owns its physical slot until it returns.
    in_flight: bool,
    error: Option<String>,
}

struct Read {
    query: Query,
    owner_query: Query,
    generation: u64,
    ticket: QueryTicket,
    offset: usize,
}

fn revisions(library: u64) -> QueryRevisions {
    QueryRevisions {
        library,
        ..QueryRevisions::default()
    }
}

impl RootStats {
    fn sync(&mut self, roots: &[PathBuf], revision: u64) -> bool {
        let query = Query::LibraryRootStats {
            roots: roots.to_vec(),
        };
        if !self.owner.sync(&query, revisions(revision)) {
            return false;
        }
        self.roots = roots.to_vec();
        self.revision = revision;
        self.restart();
        true
    }

    fn restart(&mut self) {
        self.values.clear();
        self.next = 0;
        self.error = None;
    }

    pub(super) fn cancel(&mut self) {
        self.owner.cancel();
    }

    pub(super) fn retry(&mut self) {
        self.owner.cancel();
        self.restart();
    }

    fn prepare(&mut self) -> Option<Read> {
        if self.in_flight || self.error.is_some() || self.next == self.roots.len() {
            return None;
        }
        self.in_flight = true;
        Some(Read {
            query: Query::LibraryRootStats {
                roots: self.roots[self.next..(self.next + MAX_QUERY_ROWS).min(self.roots.len())]
                    .to_vec(),
            },
            owner_query: Query::LibraryRootStats {
                roots: self.roots.clone(),
            },
            generation: self.owner.generation(),
            ticket: self.owner.ticket(),
            offset: self.next,
        })
    }

    fn current(&self, read: &Read) -> bool {
        self.owner
            .accepts(&read.owner_query, read.generation, revisions(self.revision))
    }

    fn fail(&mut self, read: &Read, error: String) {
        if self.current(read) {
            self.error = Some(error);
        }
    }

    fn complete(&mut self, read: Read, response: QueryResponse) {
        self.in_flight = false;
        if !self.current(&read) || self.error.is_some() {
            return;
        }
        if read.query.stamp(response.revisions) != read.query.stamp(revisions(self.revision)) {
            self.fail(
                &read,
                "Library changed while reading root statistics".into(),
            );
            return;
        }
        match response.result {
            Ok(ViewResponse::LibraryRootStats(values)) => {
                let Query::LibraryRootStats { roots } = &read.query else {
                    unreachable!()
                };
                if values.len() != roots.len()
                    || values
                        .iter()
                        .zip(roots)
                        .any(|(value, root)| &value.path != root)
                {
                    self.fail(&read, "Invalid library root statistics response".into());
                    return;
                }
                self.next = read.offset + values.len();
                self.values
                    .extend(values.into_iter().map(|value| (value.path.clone(), value)));
            }
            Ok(_) => self.fail(&read, "Unexpected library root statistics response".into()),
            Err(error) => self.fail(&read, error.to_string()),
        }
    }
}

fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024. && unit < UNITS.len() - 1 {
        size /= 1024.;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

impl Settings {
    pub(in crate::gui) fn sync_library_stats(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        if self.root_stats.sync(
            &self.model.draft.library_roots,
            self.handle.library_revision(),
        ) {
            cx.notify();
        }
        let Some(read) = self.root_stats.prepare() else {
            return;
        };
        let query = read.query.clone();
        let ticket = read.ticket.clone();
        let handle = self.handle.clone();
        cx.spawn(async move |this, cx| {
            let response = crate::gui::query::run(
                handle,
                cx,
                move |handle| async move { handle.query_if_current(query, ticket).await },
                |response, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if let Err(error) = response.result {
                            this.root_stats.fail(&read, error.to_string());
                            cx.notify();
                        }
                    });
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                // Refresh identity before accepting a worker that outlived its session,
                // a removed root, or a catalog commit not yet delivered to the window.
                if this.active {
                    this.root_stats.sync(
                        &this.model.draft.library_roots,
                        this.handle.library_revision(),
                    );
                }
                this.root_stats.complete(read, response);
                this.sync_library_stats(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn root_stats_label(&self, path: &Path) -> SharedString {
        if let Some(stats) = self.root_stats.values.get(path) {
            return self
                .i18n
                .format(
                    "settings-library-root-stats",
                    &[
                        ("count", stats.total.into()),
                        ("size", size_label(stats.file_size).into()),
                    ],
                )
                .into();
        }
        self.tr(if self.root_stats.error.is_some() {
            "settings-library-root-stats-unavailable"
        } else {
            "settings-library-root-stats-loading"
        })
    }

    pub(super) fn root_stats_error(&self) -> Option<&str> {
        self.root_stats.error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(read: &Read, revision: u64, total: usize) -> QueryResponse {
        let Query::LibraryRootStats { roots } = &read.query else {
            unreachable!()
        };
        QueryResponse {
            revisions: revisions(revision),
            result: Ok(ViewResponse::LibraryRootStats(
                roots
                    .iter()
                    .map(|path| LibraryRootStats {
                        path: path.clone(),
                        total,
                        file_size: 1024,
                    })
                    .collect(),
            )),
        }
    }

    #[test]
    fn root_stats_fences_removed_roots_revisions_and_closed_sessions() {
        let mut stats = RootStats::default();
        stats.sync(&["/music".into()], 1);
        let read = stats.prepare().unwrap();
        stats.sync(&["/replacement".into()], 2);
        assert!(stats.prepare().is_none());
        let old = response(&read, 1, 99);
        stats.complete(read, old);
        assert!(stats.values.is_empty());
        let read = stats.prepare().unwrap();
        let current = response(&read, 2, 3);
        stats.complete(read, current);
        assert_eq!(stats.values[Path::new("/replacement")].total, 3);
        stats.sync(&["/replacement".into()], 3);
        let read = stats.prepare().unwrap();
        stats.cancel();
        let closed = response(&read, 3, 99);
        stats.complete(read, closed);
        assert!(stats.values.is_empty());
    }

    #[test]
    fn root_stats_batches_and_timeout_retains_worker_slot() {
        let mut stats = RootStats::default();
        let roots = (0..MAX_QUERY_ROWS + 1)
            .map(|id| PathBuf::from(format!("/root/{id}")))
            .collect::<Vec<_>>();
        stats.sync(&roots, 1);
        let read = stats.prepare().unwrap();
        let first = response(&read, 1, 1);
        stats.complete(read, first);
        assert_eq!(stats.values.len(), MAX_QUERY_ROWS);
        let read = stats.prepare().unwrap();
        stats.fail(&read, "Timed out".into());
        assert!(stats.in_flight);
        assert!(stats.prepare().is_none());
        stats.sync(&roots, 2);
        assert!(stats.prepare().is_none());
        let late = response(&read, 1, 99);
        stats.complete(read, late);
        assert!(stats.values.is_empty());
        assert!(stats.prepare().is_some());
    }

    #[test]
    fn root_stats_distinguishes_zero_failure_and_reopen() {
        let mut stats = RootStats::default();
        stats.sync(&["/empty".into()], 1);
        let read = stats.prepare().unwrap();
        let zero = response(&read, 1, 0);
        stats.complete(read, zero);
        assert_eq!(stats.values[Path::new("/empty")].total, 0);
        assert!(stats.error.is_none());
        assert!(stats.prepare().is_none());
        stats.retry();
        let read = stats.prepare().unwrap();
        let invalid = QueryResponse {
            revisions: revisions(1),
            result: Ok(ViewResponse::LibraryRootStats(Vec::new())),
        };
        stats.complete(read, invalid);
        assert!(stats.values.is_empty());
        assert!(stats.error.is_some());
        assert!(stats.prepare().is_none());
        stats.retry();
        assert!(stats.error.is_none());
        assert!(stats.prepare().is_some());
    }

    #[test]
    fn root_stats_reject_worker_revisions_and_busy_failures() {
        let mut stats = RootStats::default();
        stats.sync(&["/music".into()], 1);
        let read = stats.prepare().unwrap();
        let stale = response(&read, 2, 99);
        stats.complete(read, stale);
        assert!(stats.values.is_empty());
        assert!(stats.error.is_some());
        assert!(!stats.in_flight);
        assert!(stats.prepare().is_none());

        stats.retry();
        let read = stats.prepare().unwrap();
        stats.complete(
            read,
            QueryResponse {
                revisions: revisions(1),
                result: Err(rivu_protocol::response::QueryError::Message(
                    "Query worker busy".into(),
                )),
            },
        );
        assert!(stats.values.is_empty());
        assert!(
            stats
                .error
                .as_deref()
                .unwrap()
                .contains("Query worker busy")
        );
        assert!(!stats.in_flight);
        assert!(stats.prepare().is_none());
    }

    #[test]
    fn root_stats_reject_wrong_root_identity_with_current_revision() {
        let mut stats = RootStats::default();
        stats.sync(&["/music".into()], 1);
        let read = stats.prepare().unwrap();
        let mut invalid = response(&read, 1, 10);
        let Ok(ViewResponse::LibraryRootStats(values)) = &mut invalid.result else {
            unreachable!()
        };
        values[0].path = "/other".into();
        stats.complete(read, invalid);
        assert!(stats.values.is_empty());
        assert!(stats.error.is_some());
    }

    #[test]
    fn root_stats_retry_waits_for_physical_worker_and_rejects_its_late_result() {
        let mut stats = RootStats::default();
        stats.sync(&["/music".into()], 1);
        let read = stats.prepare().unwrap();
        stats.fail(&read, "Timed out".into());
        stats.retry();
        assert!(stats.error.is_none());
        assert!(stats.in_flight);
        assert!(stats.prepare().is_none());
        let late = response(&read, 1, 99);
        stats.complete(read, late);
        assert!(stats.values.is_empty());
        let read = stats.prepare().unwrap();
        let current = response(&read, 1, 3);
        stats.complete(read, current);
        assert_eq!(stats.values[Path::new("/music")].total, 3);
    }

    #[test]
    fn root_stats_sizes_use_iec_units_and_preserve_zero() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1024), "1.0 KiB");
        assert_eq!(size_label(1536), "1.5 KiB");
        assert_eq!(size_label(1024 * 1024 * 1024), "1.0 GiB");
    }
}
