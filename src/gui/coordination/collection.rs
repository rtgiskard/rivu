//! Collection pagination and logical cancellation with a physical worker gate.

use anyhow::Result;
use rivu_protocol::{model::Query, response::ViewResponse};
use std::future::Future;

pub(super) struct CollectionRead {
    worker: u64,
    pub(super) ticket: crate::query_tracking::QueryTicket,
}

// Only one collection worker may run at a time. Cancellation retires its read
// identity, not its physical gate; only that worker's completion releases it.
#[derive(Default)]
pub(in crate::gui) struct CollectionResolution {
    epoch: crate::query_tracking::QueryEpoch,
    worker: Option<u64>,
}

impl CollectionResolution {
    pub(in crate::gui) fn pending(&self) -> bool {
        self.worker.is_some()
    }

    pub(super) fn begin(&mut self) -> Option<CollectionRead> {
        if self.pending() {
            return None;
        }
        self.epoch.advance();
        let worker = self.epoch.generation();
        self.worker = Some(worker);
        Some(CollectionRead {
            worker,
            ticket: self.epoch.ticket(),
        })
    }

    pub(in crate::gui) fn cancel(&mut self) {
        self.epoch.cancel();
    }

    pub(super) fn timeout(&mut self, read: &CollectionRead) -> bool {
        if self.worker != Some(read.worker) || !read.ticket.is_current() {
            return false;
        }
        self.cancel();
        true
    }

    pub(super) fn finish(&mut self, read: &CollectionRead) -> bool {
        if self.worker != Some(read.worker) {
            return false;
        }
        self.worker = None;
        true
    }
}

async fn query_collection_page<F: Future<Output = rivu_protocol::response::QueryResponse>>(
    query_page: &mut impl FnMut(Query, crate::query_tracking::QueryTicket) -> F,
    query: Query,
    ticket: &crate::query_tracking::QueryTicket,
) -> Result<rivu_protocol::response::QueryResponse> {
    anyhow::ensure!(
        ticket.is_current(),
        rivu_protocol::response::QueryError::Cancelled
    );
    let response = query_page(query, ticket.clone()).await;
    anyhow::ensure!(
        ticket.is_current(),
        rivu_protocol::response::QueryError::Cancelled
    );
    Ok(response)
}

// Each request is bounded; no subset is published before the complete collection succeeds.
pub(super) async fn resolve_album<F: Future<Output = rivu_protocol::response::QueryResponse>>(
    mut query_page: impl FnMut(Query, crate::query_tracking::QueryTicket) -> F,
    key: rivu_protocol::model::AlbumKey,
    expected: rivu_protocol::model::QueryStamp,
    ticket: &crate::query_tracking::QueryTicket,
) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    let mut total = None;
    let scope = Query::AlbumTracks {
        key,
        query: None,
        offset: 0,
        limit: rivu_protocol::model::MAX_QUERY_ROWS,
    };
    loop {
        let mut query = scope.clone();
        if let Query::AlbumTracks { offset, .. } = &mut query {
            *offset = ids.len();
        }
        let response = query_collection_page(&mut query_page, query, ticket).await?;
        let result = response.result?;
        anyhow::ensure!(
            scope.stamp(response.revisions) == expected,
            "collection changed"
        );
        let ViewResponse::AlbumTracks(page) = result else {
            anyhow::bail!("unexpected album response");
        };
        anyhow::ensure!(
            total.is_none_or(|total| total == page.total),
            "collection changed"
        );
        total = Some(page.total);
        anyhow::ensure!(
            valid_page(ids.len(), page.rows.len(), page.total),
            "incomplete album response"
        );
        ids.extend(page.rows.into_iter().map(|track| track.id));
        if ids.len() == page.total {
            return Ok(ids);
        }
    }
}

fn valid_page(offset: usize, rows: usize, total: usize) -> bool {
    offset <= total
        && rows <= rivu_protocol::model::MAX_QUERY_ROWS
        && rows <= total - offset
        && (rows > 0 || offset == total)
}

#[cfg(test)]
mod tests {
    use super::{CollectionResolution, resolve_album, valid_page};
    use crate::query_tracking::QueryEpoch;
    use rivu_protocol::{
        model::Query,
        response::{QueryError, QueryRevisions, ViewResponse},
    };
    use std::path::PathBuf;

    #[test]
    fn collection_timeout_retires_logical_owner_but_not_physical_worker() {
        {
            let mut resolution = CollectionResolution::default();
            let read = resolution.begin().unwrap();
            assert!(resolution.timeout(&read));
            assert!(!read.ticket.is_current());
            assert!(resolution.pending());
            assert!(resolution.begin().is_none());
            assert!(!resolution.timeout(&read));
            assert!(resolution.finish(&read));
            let latest = resolution.begin().unwrap();
            assert!(!resolution.timeout(&read));
            assert!(!resolution.finish(&read));
            assert!(latest.ticket.is_current());
            resolution.cancel();
            assert!(!resolution.timeout(&latest));
            assert!(resolution.pending());
            assert!(resolution.finish(&latest));
        }
    }

    #[test]
    fn collection_timeout_during_pagination_discards_late_pages_and_partial_ids() {
        let Query::AlbumTracks { key, .. } = album_query() else {
            unreachable!()
        };
        let revisions = QueryRevisions::default();
        for timeout_at in 1..=2 {
            let mut resolution = CollectionResolution::default();
            let read = resolution.begin().unwrap();
            let mut calls = 0;
            let result = futures::executor::block_on(resolve_album(
                |_, _| {
                    calls += 1;
                    if calls == timeout_at {
                        assert!(resolution.timeout(&read));
                        assert!(resolution.pending());
                    }
                    futures::future::ready(album_page(&[calls as i64], 2, revisions))
                },
                key.clone(),
                album_query().stamp(revisions),
                &read.ticket,
            ));
            assert!(matches!(
                result.unwrap_err().downcast_ref::<QueryError>(),
                Some(QueryError::Cancelled)
            ));
            assert_eq!(calls, timeout_at);
            assert!(resolution.pending());
            assert!(resolution.finish(&read));
        }
    }

    #[test]
    fn album_cancellation_drains_a_pending_response_before_releasing_the_gate() {
        futures::executor::block_on(async {
            let Query::AlbumTracks { key, .. } = album_query() else {
                unreachable!()
            };
            let revisions = QueryRevisions::default();
            let mut resolution = CollectionResolution::default();
            let read = resolution.begin().unwrap();
            let (sender, receiver) = futures::channel::oneshot::channel();
            let mut receiver = Some(receiver);
            let mut calls = 0;
            let result = {
                let operation = resolve_album(
                    |_, ticket| {
                        assert!(ticket.is_current());
                        calls += 1;
                        let receiver = receiver.take().unwrap();
                        async move { receiver.await.unwrap() }
                    },
                    key,
                    album_query().stamp(revisions),
                    &read.ticket,
                );
                futures::pin_mut!(operation);
                assert!(futures::poll!(operation.as_mut()).is_pending());
                resolution.cancel();
                assert!(resolution.pending());
                assert!(resolution.begin().is_none());
                assert!(futures::poll!(operation.as_mut()).is_pending());
                sender.send(album_page(&[1], 2, revisions)).unwrap();
                operation.await
            };
            assert!(matches!(
                result.unwrap_err().downcast_ref::<QueryError>(),
                Some(QueryError::Cancelled)
            ));
            assert_eq!(calls, 1);
            assert!(resolution.pending());
            assert!(resolution.finish(&read));
            assert!(!resolution.pending());
        });
    }

    #[test]
    fn collection_pages_reject_partial_and_overflowing_results() {
        assert!(valid_page(0, 0, 0));
        assert!(valid_page(3, 2, 5));
        assert!(!valid_page(3, 0, 5));
        assert!(!valid_page(3, 3, 5));
        assert!(!valid_page(6, 0, 5));
        assert!(!valid_page(
            0,
            rivu_protocol::model::MAX_QUERY_ROWS + 1,
            usize::MAX
        ));
    }

    fn album_query() -> Query {
        Query::AlbumTracks {
            key: rivu_protocol::model::AlbumKey {
                directory: PathBuf::from("/album"),
                title: "Album".into(),
            },
            query: None,
            offset: 0,
            limit: rivu_protocol::model::MAX_QUERY_ROWS,
        }
    }

    fn album_page(
        ids: &[i64],
        total: usize,
        revisions: rivu_protocol::response::QueryRevisions,
    ) -> rivu_protocol::response::QueryResponse {
        rivu_protocol::response::QueryResponse {
            revisions,
            result: Ok(ViewResponse::AlbumTracks(rivu_protocol::model::Page {
                total,
                rows: ids
                    .iter()
                    .map(|id| rivu_protocol::model::LibraryRow {
                        id: *id,
                        path: Default::default(),
                        title: String::new(),
                        artist: String::new(),
                        album: String::new(),
                        duration: None,
                        codec: String::new(),
                        bitrate_bps: None,
                        bitrate_exact: false,
                        channels: 0,
                        sample_rate: 0,
                        bits_per_sample: None,
                        cue_number: None,
                        favorite: false,
                        missing: false,
                        play_count: 0,
                    })
                    .collect(),
            })),
        }
    }

    #[test]
    fn album_resolution_keeps_complete_order_across_pages() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = rivu_protocol::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        let mut offsets = Vec::new();
        let mut epoch = QueryEpoch::default();
        let ticket = epoch.ticket();
        let ids = futures::executor::block_on(resolve_album(
            |query, _| {
                let Query::AlbumTracks { offset, limit, .. } = query else {
                    unreachable!()
                };
                offsets.push(offset);
                assert_eq!(limit, rivu_protocol::model::MAX_QUERY_ROWS);
                futures::future::ready(if offset == 0 {
                    album_page(&[9, 2], 4, revisions)
                } else {
                    album_page(&[8, 3], 4, revisions)
                })
            },
            key,
            query.stamp(revisions),
            &ticket,
        ))
        .unwrap();
        assert_eq!(offsets, [0, 2]);
        assert_eq!(ids, [9, 2, 8, 3]);
    }

    #[test]
    fn album_resolution_never_returns_partial_stale_or_failed_pages() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = rivu_protocol::response::QueryRevisions {
            library: 8,
            structure: 4,
            ..Default::default()
        };
        for failure in 0..3 {
            let mut calls = 0;
            let mut epoch = QueryEpoch::default();
            let ticket = epoch.ticket();
            let result = futures::executor::block_on(resolve_album(
                |_, _| {
                    calls += 1;
                    if calls == 1 {
                        return futures::future::ready(album_page(&[9, 2], 4, revisions));
                    }
                    futures::future::ready(match failure {
                        0 => album_page(
                            &[8, 3],
                            4,
                            rivu_protocol::response::QueryRevisions {
                                album: 1,
                                ..revisions
                            },
                        ),
                        1 => album_page(&[], 4, revisions),
                        _ => rivu_protocol::response::QueryResponse {
                            revisions,
                            result: Err(rivu_protocol::response::QueryError::Message(
                                "read failed".into(),
                            )),
                        },
                    })
                },
                key.clone(),
                query.stamp(revisions),
                &ticket,
            ));
            assert!(result.is_err());
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn menu_cancellation_keeps_physical_gate_until_worker_finishes() {
        let mut resolution = CollectionResolution::default();
        let read = resolution.begin().unwrap();
        resolution.cancel();
        assert!(!read.ticket.is_current());
        assert!(resolution.pending());
        assert!(resolution.begin().is_none());
        assert!(resolution.finish(&read));
        let next = resolution.begin().unwrap();
        assert!(!resolution.finish(&read));
        assert!(next.ticket.is_current());
        assert!(resolution.finish(&next));
    }

    #[test]
    fn destroying_collection_owner_cancels_menu_resolution() {
        let mut resolution = CollectionResolution::default();
        let read = resolution.begin().unwrap();
        drop(resolution);
        assert!(!read.ticket.is_current());
    }

    #[test]
    fn album_cancellation_stops_pages_without_publishing_or_releasing_worker() {
        let query = album_query();
        let Query::AlbumTracks { key, .. } = query.clone() else {
            unreachable!()
        };
        let revisions = QueryRevisions::default();
        {
            // Cancel before dispatch, during an intermediate page, or during the final page.
            for cancel_at in 0..=2 {
                let mut resolution = CollectionResolution::default();
                let read = resolution.begin().unwrap();
                if cancel_at == 0 {
                    resolution.cancel();
                }
                let mut calls = 0;
                let result = futures::executor::block_on(resolve_album(
                    |_, ticket| {
                        assert!(ticket.is_current());
                        calls += 1;
                        if calls == cancel_at {
                            resolution.cancel();
                        }
                        futures::future::ready(album_page(&[calls as i64], 2, revisions))
                    },
                    key.clone(),
                    query.stamp(revisions),
                    &read.ticket,
                ));
                assert!(matches!(
                    result.unwrap_err().downcast_ref::<QueryError>(),
                    Some(QueryError::Cancelled)
                ));
                assert_eq!(calls, cancel_at);
                assert!(resolution.pending());
                assert!(resolution.begin().is_none());
                assert!(resolution.finish(&read));
                assert!(!resolution.pending());
            }
        }
    }
}
