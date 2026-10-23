use super::*;

#[test]
fn cancelled_queued_read_skips_store_and_does_not_cancel_another_owner() {
    let (directory, core) = fixture_with_database(Some(10.0), true);
    core.store.set_track_cache_page_size(0);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute("UPDATE tracks SET channels='unreadable' WHERE id=1", [])
        .unwrap();
    assert!(core.view(&Query::Track { track_id: 1 }).is_err());

    let mut cancelled = QueryEpoch::default();
    let mut current = QueryEpoch::default();
    let (sender, requests) = mpsc::channel(3);
    let enqueue = |track_id, ticket| {
        let (reply, response) = oneshot::channel();
        sender
            .blocking_send(Request {
                operation: Operation::Query {
                    query: Query::Track { track_id },
                    ticket,
                },
                reply: Some(reply),
            })
            .unwrap();
        response
    };
    let stale = enqueue(1, Some(cancelled.ticket()));
    let independent = enqueue(2, Some(current.ticket()));
    let unconditional = enqueue(1, None);
    cancelled.cancel();
    drop(sender);
    drop(run_until_disconnected(core, requests));

    assert!(matches!(
        stale.blocking_recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Err(QueryError::Cancelled),
            ..
        })
    ));
    assert!(matches!(
        independent.blocking_recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Ok(ViewResponse::Track(Some(track))),
            ..
        }) if track.id == 2
    ));
    assert!(matches!(
        unconditional.blocking_recv().unwrap(),
        CoreResponse::Query(QueryResponse {
            result: Err(QueryError::Message(_)),
            ..
        })
    ));
}

#[test]
fn already_cancelled_read_never_enters_the_request_queue() {
    let (handle, _analysis, mut requests) = display_fixture();
    let mut epoch = QueryEpoch::default();
    let ticket = epoch.ticket();
    epoch.cancel();
    assert!(matches!(
        handle
            .query_if_current(Query::Track { track_id: 1 }, ticket)
            .result,
        Err(QueryError::Cancelled)
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn cancellation_after_dispatch_waits_for_the_physical_reply() {
    use std::sync::atomic::AtomicBool;

    let (_directory, core) = fixture();
    let (handle, _analysis, mut requests) = display_fixture();
    let mut epoch = QueryEpoch::default();
    let ticket = epoch.ticket();
    let gate = Arc::new(AtomicBool::new(true));
    let worker_gate = Arc::clone(&gate);
    let (finished, completion) = bounded(1);
    let worker = thread::spawn(move || {
        let response = handle.query_if_current(Query::Track { track_id: 1 }, ticket);
        worker_gate.store(false, Ordering::Release);
        finished.send(response).unwrap();
    });
    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let Operation::Query { query, ticket } = request.operation else {
        panic!("expected a read request");
    };
    // Execute the same dispatch boundary as Core::run, then deliberately
    // hold physical completion delivery until after cancellation.
    let response = core.query_response_if_current(&query, ticket.as_ref());
    assert!(matches!(&response.result, Ok(ViewResponse::Track(Some(track))) if track.id == 1));
    epoch.cancel();
    assert!(gate.load(Ordering::Acquire));
    assert!(completion.try_recv().is_err());
    request
        .reply
        .unwrap()
        .send(CoreResponse::Query(response))
        .unwrap();
    assert!(matches!(
        completion.recv_timeout(Duration::from_secs(2)).unwrap().result,
        Ok(ViewResponse::Track(Some(track))) if track.id == 1
    ));
    worker.join().unwrap();
    assert!(!gate.load(Ordering::Acquire));
}

#[test]
fn gui_timeout_notifies_without_retiring_and_discards_late_reply_or_disconnect() {
    for disconnect in [false, true] {
        let (handle, _analysis, mut requests) = display_fixture();
        let (notice, notices) = bounded(1);
        let handle = handle.with_query_timeout(move |response| notice.send(response).unwrap());
        let mut epoch = QueryEpoch::default();
        let ticket = epoch.ticket();
        let (finished, completion) = bounded(1);
        let worker = thread::spawn(move || {
            finished
                .send(handle.query_with_ticket(
                    Query::Track { track_id: 1 },
                    Some(ticket),
                    true,
                    Duration::ZERO,
                ))
                .unwrap();
        });
        let request = requests.blocking_recv().unwrap();
        let notification = notices.recv().unwrap();
        assert!(matches!(notification.result, Err(QueryError::Message(_))));
        // Logical replacement/cancellation cannot complete the submitted worker.
        epoch.cancel();
        let latest = epoch.ticket();
        assert!(latest.is_current());
        assert!(completion.try_recv().is_err());
        assert!(requests.try_recv().is_err());
        if !disconnect {
            request
                .reply
                .unwrap()
                .send(CoreResponse::Query(QueryResponse {
                    revisions: QueryRevisions::default(),
                    result: Ok(ViewResponse::Track(None)),
                }))
                .unwrap();
        } else {
            drop(request);
        }
        let result = completion.recv().unwrap();
        assert!(matches!(result.result, Err(QueryError::Message(_))));
        assert!(notices.try_recv().is_err());
        worker.join().unwrap();
    }
}

#[test]
fn non_gui_timeout_does_not_wait_for_physical_retirement_or_notify_gui() {
    let (handle, _analysis, mut requests) = display_fixture();
    let handle = handle.with_query_timeout(|_| panic!("non-GUI query notified GUI"));
    let result =
        handle.query_with_ticket(Query::Track { track_id: 1 }, None, false, Duration::ZERO);
    assert!(matches!(result.result, Err(QueryError::Message(_))));
    let request = requests.blocking_recv().unwrap();
    assert!(
        request
            .reply
            .unwrap()
            .send(CoreResponse::Query(QueryResponse {
                revisions: QueryRevisions::default(),
                result: Ok(ViewResponse::Track(None)),
            }))
            .is_err()
    );
}

#[test]
fn directory_missing_query_composes_filters_ancestors_and_exact_pages() {
    use crate::model::DirectoryRow;
    let (directory, core) = fixture_with_database(Some(10.0), true);
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "UPDATE tracks SET path='/music/a/deep/missing.wav', raw_title='Needle Nested', favorite=1, missing=1 WHERE id=1;
             UPDATE tracks SET path='/music/a/deep/present.wav', raw_title='Needle Present', favorite=1, missing=0 WHERE id=2;
             UPDATE tracks SET path='/music/b/deep/missing.wav', raw_title='Needle Other', favorite=0, missing=1 WHERE id=3;
             UPDATE tracks SET path='/music/direct.wav', raw_title='Needle Direct', favorite=1, missing=1 WHERE id=4;
             INSERT INTO tracks(id,path,raw_title,favorite,missing) VALUES
             (5,'/music/present/only.wav','Other',0,0),
             (6,'/music/hidden/deep/missing.wav','Other',1,1);",
        )
        .unwrap();
    let page = |path: &str, query: Option<&str>, favorite, missing, offset, limit| {
        let response = core.query_response(&Query::DirectoryPage {
            path: path.into(),
            query: query.map(str::to_owned),
            favorite,
            missing,
            offset,
            limit,
        });
        let Ok(ViewResponse::DirectoryPage(page)) = response.result else {
            panic!("expected directory page");
        };
        page
    };
    let directory_row = |path: &str| DirectoryRow::Directory { path: path.into() };
    for (missing, paths, track_ids) in [
        (true, vec!["/music/a", "/music/b", "/music/hidden"], vec![4]),
        (false, vec!["/music/a", "/music/present"], vec![]),
    ] {
        let all = page("/music", None, None, Some(missing), 0, 10);
        assert_eq!(all.total, paths.len() + track_ids.len());
        assert_eq!(
            &all.rows[..paths.len()],
            paths
                .iter()
                .map(|path| directory_row(path))
                .collect::<Vec<_>>()
        );
        for (row, id) in all.rows[paths.len()..].iter().zip(track_ids) {
            assert!(
                matches!(row, DirectoryRow::Track(row) if row.id == id && row.missing == missing)
            );
        }
        for offset in 0..all.total {
            let single = page("/music", None, None, Some(missing), offset, 1);
            assert_eq!(single.total, all.total);
            assert_eq!(single.rows, all.rows[offset..offset + 1]);
        }
        for (offset, limit) in [(0, 0), (all.total, 10), (usize::MAX, 10)] {
            let empty = page("/music", None, None, Some(missing), offset, limit);
            assert_eq!(empty.total, all.total);
            assert!(empty.rows.is_empty());
        }
    }
    for missing in [true, false] {
        let selected = page("/music", Some("NEEDLE"), Some(true), Some(missing), 0, 10);
        assert_eq!(selected.total, if missing { 2 } else { 1 });
        assert_eq!(selected.rows[0], directory_row("/music/a"));
        if missing {
            assert!(
                matches!(&selected.rows[1], DirectoryRow::Track(row) if row.id == 4 && row.favorite && row.missing)
            );
        }
        let ancestor = page("/music/a", Some("needle"), Some(true), Some(missing), 0, 10);
        assert_eq!(ancestor.total, 1);
        assert_eq!(ancestor.rows, [directory_row("/music/a/deep")]);
        let leaf = page(
            "/music/a/deep",
            Some("needle"),
            Some(true),
            Some(missing),
            0,
            10,
        );
        assert_eq!(leaf.total, 1);
        let expected_id = if missing { 1 } else { 2 };
        assert!(
            matches!(&leaf.rows[..], [DirectoryRow::Track(row)] if row.id == expected_id && row.favorite && row.missing == missing)
        );
        let count = page("/music", Some("needle"), Some(true), Some(missing), 0, 0);
        assert_eq!(count.total, selected.total);
        assert!(count.rows.is_empty());
        let absent = page("/music", Some("absent"), Some(true), Some(missing), 0, 10);
        assert_eq!(absent.total, 0);
        assert!(absent.rows.is_empty());
    }
    let non_favorite = page("/music", Some("needle"), Some(false), Some(true), 0, 10);
    assert_eq!(non_favorite.total, 1);
    assert_eq!(non_favorite.rows, [directory_row("/music/b")]);
    let absent = page("/music/hidden", None, None, Some(false), 0, 10);
    assert_eq!(absent.total, 0);
    assert!(absent.rows.is_empty());
}

#[test]
fn batch_continuation_rejects_library_change_but_not_playback_change() {
    let (_directory, mut core) = fixture();
    let expected = core.state.query_revisions();
    let ViewResponse::TrackBatch(first) = core
        .view(&Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 2,
            expected: Some(expected),
        })
        .unwrap()
    else {
        panic!("expected track batch");
    };
    let query = Query::TrackBatch {
        query: None,
        favorite: None,
        missing: None,
        after: first.next,
        limit: 2,
        expected: Some(expected),
    };
    core.state.playback.position = 2.0;
    core.publish();
    let ViewResponse::TrackBatch(last) = core.view(&query).unwrap() else {
        panic!("expected track batch");
    };
    assert_eq!(
        last.rows.iter().map(|track| track.id).collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(last.next, None);
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
}

#[test]
fn batch_errors_distinguish_revision_changes_from_invalid_requests() {
    let (_directory, mut core) = fixture();
    core.command(Command::CreatePlaylistWithTracks {
        name: "mix".into(),
        track_ids: vec![1, 2],
    })
    .unwrap();
    let expected = core.state.query_revisions();
    let entry_expected = core.state.playlist_query_revisions(1);
    let queries = [
        Query::TrackBatch {
            query: None,
            favorite: None,
            missing: None,
            after: None,
            limit: 0,
            expected: Some(expected),
        },
        Query::PlaylistSummaryBatch {
            query: None,
            after: None,
            limit: 0,
            expected: Some(expected),
        },
        Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 0,
            expected: Some(entry_expected),
        },
    ];
    for query in &queries {
        assert!(matches!(core.view(query), Err(QueryError::Message(_))));
    }
    core.command(Command::RenamePlaylist {
        playlist_id: 1,
        name: "renamed".into(),
    })
    .unwrap();
    // A stale relevant dependency wins over the invalid limit. Rename changes
    // only summaries; track and entry rows still report the invalid request.
    assert_eq!(
        core.view(&queries[1]).unwrap_err(),
        QueryError::RevisionChanged
    );
    for query in [&queries[0], &queries[2]] {
        assert!(matches!(core.view(query), Err(QueryError::Message(_))));
    }
    let renamed = core.state.query_revisions();
    core.command(Command::SetFavorite {
        track_ids: vec![1],
        favorite: true,
    })
    .unwrap();
    for query in &queries {
        assert_eq!(core.view(query).unwrap_err(), QueryError::RevisionChanged);
    }
    // With a current catalog baseline, stale L alone is irrelevant to summaries.
    assert!(matches!(
        core.view(&Query::PlaylistSummaryBatch {
            query: None,
            after: None,
            limit: 0,
            expected: Some(renamed),
        }),
        Err(QueryError::Message(_))
    ));
    let entry_expected = core.state.playlist_query_revisions(1);
    let entry_id = core
        .store
        .playlist_entries_page(1, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry { entry_id, index: 1 })
        .unwrap();
    // Entry content staleness also takes priority independently of metadata.
    assert_eq!(
        core.view(&Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 1,
            after: None,
            limit: 0,
            expected: Some(entry_expected),
        })
        .unwrap_err(),
        QueryError::RevisionChanged
    );
}

#[test]
fn queue_batch_revisions_ignore_current_only_changes_and_reject_metadata_edits() {
    let (_directory, mut core) = fixture();
    let expected = core.state.query_revisions();
    core.state.queue.current_id = Some(3);
    core.save_queue().unwrap();
    let query = Query::QueueBatch {
        query: None,
        after: None,
        limit: 2,
        expected: Some(expected),
    };
    assert!(matches!(core.view(&query), Ok(ViewResponse::QueueBatch(_))));
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("Changed".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert!(matches!(
        core.view(&query),
        Err(QueryError::RevisionChanged)
    ));
    let state = core.state.query_revisions();
    core.state.playback.position = 5.;
    assert_eq!(core.state.query_revisions().queue, state.queue);
}

#[test]
fn filtered_queue_continuations_bind_rank_query_and_revision_without_expected() {
    let (_directory, mut core) = fixture();
    for track_id in [1, 2, 4] {
        core.command(Command::EditTrack {
            track_id,
            title: Some("needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
    }
    let batch = |filter: Option<&str>, after| Query::QueueBatch {
        query: filter.map(str::to_owned),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first = None;
    let mut rows = Vec::new();
    loop {
        let ViewResponse::QueueBatch(page) = core.view(&batch(Some("needle"), after)).unwrap()
        else {
            panic!("expected queue batch")
        };
        assert_eq!(page.rows[0].offset, rows.len());
        after = page.next;
        if first.is_none() {
            first = after;
        }
        rows.extend(page.rows);
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, rows.len());
    }
    assert_eq!(
        rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        [0, 1, 2, 4]
    );
    assert_eq!(rows[1].track_id, rows[2].track_id);
    assert_ne!(rows[1].id, rows[2].id);
    let ViewResponse::QueuePage(jump) = core
        .view(&Query::QueuePage {
            query: Some("needle".into()),
            offset: 2,
            limit: 1,
        })
        .unwrap()
    else {
        panic!("expected queue page")
    };
    assert_eq!(jump.total, 4);
    assert_eq!(jump.rows, rows[2..3]);
    for filter in [Some("track"), None] {
        assert_eq!(
            core.view(&batch(filter, first)).unwrap_err(),
            QueryError::RevisionChanged
        );
    }
    // Playback selection alone preserves the matching continuation.
    core.state.queue.current_id = Some(rows[2].id);
    core.save_queue().unwrap();
    assert!(core.view(&batch(Some("NEEDLE"), first)).is_ok());
    let mut overflow = first.unwrap();
    overflow.offset = usize::MAX;
    assert!(matches!(
        core.view(&batch(Some("needle"), Some(overflow))),
        Err(QueryError::Message(_))
    ));
    core.enqueue(&[2]).unwrap();
    assert_eq!(
        core.view(&batch(Some("needle"), first)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::QueueBatch(fresh) = core.view(&batch(Some("needle"), None)).unwrap() else {
        panic!("expected queue batch")
    };
    core.command(Command::EditTrack {
        track_id: 2,
        title: Some("not matched".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(Some("needle"), fresh.next)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::QueueBatch(restarted) = core.view(&batch(Some("needle"), None)).unwrap()
    else {
        panic!("expected queue batch")
    };
    assert_eq!(restarted.rows[0].offset, 0);
}

#[test]
fn filtered_playlist_continuations_reject_foreign_query_metadata_and_order() {
    let (_directory, mut core) = fixture();
    for track_id in [1, 3, 4] {
        core.command(Command::EditTrack {
            track_id,
            title: Some("needle".into()),
            artist: None,
            album: None,
            decoder_hint: None,
        })
        .unwrap();
    }
    for name in ["first", "second"] {
        core.command(Command::CreatePlaylistWithTracks {
            name: name.into(),
            track_ids: vec![1, 2, 3, 4],
        })
        .unwrap();
    }
    let batch = |playlist_id, filter: Option<&str>, after| Query::PlaylistEntryBatch {
        playlist_id,
        query: filter.map(str::to_owned),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first = None;
    let mut rows = Vec::new();
    loop {
        let ViewResponse::PlaylistEntryBatch(page) =
            core.view(&batch(1, Some("needle"), after)).unwrap()
        else {
            panic!("expected playlist batch")
        };
        assert_eq!(page.rows[0].offset, rows.len());
        after = page.next;
        if first.is_none() {
            first = after;
        }
        rows.extend(page.rows);
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, rows.len());
    }
    assert_eq!(
        rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        [0, 2, 3]
    );
    for query in [
        batch(2, Some("needle"), first),
        batch(1, Some("track"), first),
        batch(1, None, first),
    ] {
        assert_eq!(core.view(&query).unwrap_err(), QueryError::RevisionChanged);
    }
    let other = core
        .store
        .playlist_entries_page(2, None, 0, 1)
        .unwrap()
        .rows[0]
        .id;
    core.command(Command::MovePlaylistEntry {
        entry_id: other,
        index: 3,
    })
    .unwrap();
    assert!(core.view(&batch(1, Some("needle"), first)).is_ok());
    core.command(Command::MovePlaylistEntry {
        entry_id: rows[0].id,
        index: 3,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(1, Some("needle"), first)).unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(fresh) =
        core.view(&batch(1, Some("needle"), None)).unwrap()
    else {
        panic!("expected playlist batch")
    };
    assert_eq!(
        (
            fresh.rows[0].track_id,
            fresh.rows[0].position,
            fresh.rows[0].offset
        ),
        (3, 1, 0)
    );
    core.command(Command::EditTrack {
        track_id: 3,
        title: Some("not matched".into()),
        artist: None,
        album: None,
        decoder_hint: None,
    })
    .unwrap();
    assert_eq!(
        core.view(&batch(1, Some("needle"), fresh.next))
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(restarted) =
        core.view(&batch(1, Some("needle"), None)).unwrap()
    else {
        panic!("expected playlist batch")
    };
    assert_eq!(
        (
            restarted.rows[0].track_id,
            restarted.rows[0].position,
            restarted.rows[0].offset
        ),
        (4, 2, 0)
    );
}
