use super::*;
use crate::{
    app::{AppCommand, AppHandle},
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use std::sync::Arc;

#[test]
fn app_commands_route_over_ipc_without_changing_business_state() {
    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = crate::core::Runtime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(&owner, runtime.handle.clone(), app).unwrap();
    let mut client = client_session(directory.path()).unwrap();
    // A query fences asynchronous startup before comparing published state.
    client.query(&Query::ScanTask { id: 0 }).unwrap();
    let before = client.get_state(StateSections::ALL).unwrap();
    let rejected = client.request_ack(&AppCommand::ShowWindow).unwrap();
    assert!(!rejected.ok);
    assert!(
        rejected
            .error
            .as_deref()
            .unwrap()
            .contains("graphical host")
    );
    assert_eq!(rejected.revision, before.system.as_ref().unwrap().revision);
    let after = client.get_state(StateSections::ALL).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    let ack = client.request_ack(&AppCommand::Shutdown).unwrap();
    assert!(ack.ok);
    runtime.join().unwrap();
    assert!(runtime.handle.is_shutting_down());
    assert!(ack.revision <= runtime.handle.revision());
    drop(server);
    assert!(SocketOwner::acquire(directory.path()).is_err());
    drop(owner);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn scan_task_query_and_cancel_wire_round_trip_preserve_id() {
    for id in [0, 42, u64::MAX] {
        let mut encoded = Vec::new();
        encode_request_frame_into(
            &WireRequest::Query(WireQuery::from(Query::ScanTask { id })),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected query");
        };
        assert!(
            matches!(Query::try_from(wire).unwrap(), Query::ScanTask { id: decoded } if decoded == id)
        );
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(Command::CancelScan { id })),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected command");
        };
        assert!(
            matches!(Command::try_from(wire).unwrap(), Command::CancelScan { id: decoded } if decoded == id)
        );
    }
}

#[test]
fn playlist_group_requests_and_locations_round_trip_stable_identities() {
    let query = Query::PlaylistEntryLocations {
        playlist_id: 10,
        entry_ids: vec![42, i64::MAX, 42],
    };
    let expected = serde_json::to_value(&query).unwrap();
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Query(WireQuery::from(query)), &mut encoded).unwrap();
    let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("expected query");
    };
    assert_eq!(
        serde_json::to_value(Query::try_from(wire).unwrap()).unwrap(),
        expected
    );
    for command in [
        Command::RemovePlaylistEntries {
            playlist_id: 10,
            entry_ids: vec![42, i64::MAX],
            expected_revision: u64::MAX,
        },
        Command::MovePlaylistEntries {
            playlist_id: 10,
            entry_ids: vec![42, 43],
            before_entry_id: Some(44),
            expected_revision: 17,
        },
        Command::MovePlaylistEntries {
            playlist_id: 10,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: 0,
        },
    ] {
        let expected = serde_json::to_value(&command).unwrap();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(command)),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected command");
        };
        assert_eq!(
            serde_json::to_value(Command::try_from(wire).unwrap()).unwrap(),
            expected
        );
    }
    let response = QueryResponse {
        revisions: crate::response::QueryRevisions {
            library: 5,
            structure: 6,
            playlist: 7,
            queue: 8,
            playlist_entries: Some(crate::response::PlaylistEntryRevision {
                playlist_id: 10,
                revision: 9,
            }),
        },
        result: Ok(crate::response::ViewResponse::PlaylistEntryLocations(vec![
            crate::model::PlaylistEntryLocation {
                id: 42,
                playlist_id: 10,
                track_id: 11,
                position: 1000,
                total: 2000,
            },
        ])),
    };
    let expected = serde_json::to_value(&response).unwrap();
    encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
    let WireResponse::Query(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected query response");
    };
    assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
}

#[test]
fn scan_task_status_wire_round_trip_preserves_all_phases() {
    use crate::{
        model::{ScanPhase, ScanProgress, ScanTaskStatus},
        response::{QueryRevisions, ViewResponse},
    };
    for phase in [
        ScanPhase::Discovering,
        ScanPhase::ReadingMetadata,
        ScanPhase::Saving,
        ScanPhase::Completed,
        ScanPhase::Failed,
        ScanPhase::Cancelled,
    ] {
        let task = ScanTaskStatus {
            id: u64::MAX,
            progress: ScanProgress {
                phase,
                processed: 3,
                total: Some(7),
                errors: 1,
                path: Some("/音楽/song.flac".into()),
            },
            message: "task status".into(),
            error: Some("task error".into()),
        };
        let response = QueryResponse {
            revisions: QueryRevisions::default(),
            result: Ok(ViewResponse::ScanTask(task.clone())),
        };
        let mut encoded = Vec::new();
        encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
        let WireResponse::Query(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
            panic!("expected query response");
        };
        let ViewResponse::ScanTask(decoded) = decoded.result.unwrap() else {
            panic!("expected scan task");
        };
        assert_eq!(decoded, task);
    }
}

#[test]
fn directory_query_wire_round_trip_preserves_filter_and_page() {
    use std::path::Path;
    for (favorite, missing) in [None, Some(true), Some(false)]
        .into_iter()
        .flat_map(|favorite| {
            [None, Some(true), Some(false)]
                .into_iter()
                .map(move |missing| (favorite, missing))
        })
    {
        for query in [None, Some(""), Some("Été_100%")] {
            let request = WireRequest::Query(WireQuery::from(Query::DirectoryPage {
                path: "/music".into(),
                query: query.map(str::to_owned),
                favorite,
                missing,
                offset: 7,
                limit: 11,
            }));
            let mut encoded = Vec::new();
            encode_request_frame_into(&request, &mut encoded).unwrap();
            let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
                panic!("expected query");
            };
            let Query::DirectoryPage {
                path,
                query: decoded_query,
                favorite: decoded_favorite,
                missing: decoded_missing,
                offset,
                limit,
            } = Query::try_from(wire).unwrap()
            else {
                panic!("expected directory query");
            };
            assert_eq!(path, Path::new("/music"));
            assert_eq!(decoded_query.as_deref(), query);
            assert_eq!(decoded_favorite, favorite);
            assert_eq!(decoded_missing, missing);
            assert_eq!((offset, limit), (7, 11));
        }
    }
}

#[test]
fn library_root_query_wire_round_trip_preserves_paths_filters_and_pages() {
    use crate::model::LibrarySort;
    for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
        for query in [None, Some(""), Some("Été_100%")] {
            for favorite in [None, Some(true), Some(false)] {
                for missing in [None, Some(true), Some(false)] {
                    let query = Query::LibraryRootPage {
                        path: "/音楽/夏/../".into(),
                        query: query.map(str::to_owned),
                        favorite,
                        missing,
                        sort,
                        offset: 7,
                        limit: 11,
                    };
                    let expected = serde_json::to_value(&query).unwrap();
                    let mut encoded = Vec::new();
                    encode_request_frame_into(
                        &WireRequest::Query(WireQuery::from(query)),
                        &mut encoded,
                    )
                    .unwrap();
                    let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap()
                    else {
                        panic!("expected query");
                    };
                    assert_eq!(
                        serde_json::to_value(Query::try_from(wire).unwrap()).unwrap(),
                        expected
                    );
                }
            }
        }
    }
    for roots in [
        Vec::new(),
        vec![
            "/".into(),
            "/音楽".into(),
            "/音楽/夏".into(),
            "/音楽".into(),
        ],
        vec!["/music".into(); crate::model::MAX_QUERY_ROWS],
    ] {
        let query = Query::LibraryRootCounts { roots };
        let expected = serde_json::to_value(&query).unwrap();
        let mut encoded = Vec::new();
        encode_request_frame_into(&WireRequest::Query(WireQuery::from(query)), &mut encoded)
            .unwrap();
        let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected query");
        };
        assert_eq!(
            serde_json::to_value(Query::try_from(wire).unwrap()).unwrap(),
            expected
        );
    }
}

#[test]
fn library_root_responses_wire_round_trip_preserves_projection_and_revisions() {
    use crate::{
        model::{LibraryRootCount, LibraryRow, Page},
        response::{QueryRevisions, ViewResponse},
    };
    for result in [
        ViewResponse::LibraryRootCounts(vec![
            LibraryRootCount {
                path: "/音楽".into(),
                total: 3,
            },
            LibraryRootCount {
                path: "/empty".into(),
                total: 0,
            },
        ]),
        ViewResponse::LibraryRootCounts(Vec::new()),
        ViewResponse::LibraryPage(Page {
            total: 4,
            rows: vec![LibraryRow {
                id: 42,
                title: "Été_100%".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Some(12.0),
                codec: "pcm".into(),
                bitrate_bps: Some(1536000),
                bitrate_exact: true,
                favorite: true,
                missing: true,
                play_count: 7,
            }],
        }),
    ] {
        let response = QueryResponse {
            revisions: QueryRevisions {
                library: 5,
                structure: 6,
                playlist: 7,
                queue: 8,
                playlist_entries: None,
            },
            result: Ok(result),
        };
        let expected = serde_json::to_value(&response).unwrap();
        let mut encoded = Vec::new();
        encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
        let WireResponse::Query(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
            panic!("expected query response");
        };
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }
}

#[test]
fn library_root_queries_run_through_real_owner_and_ipc_session() {
    use crate::{
        config::Config,
        core::Runtime,
        model::{LibrarySort, MAX_QUERY_ROWS},
        response::{QueryError, ViewResponse},
        store::Store,
    };
    let directory = socket_test_directory();
    let root = directory.path().join("音楽");
    let nested = root.join("夏");
    let empty = directory.path().join("empty");
    let database = directory.path().join("library.db");
    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    for (path, title, missing) in [
        (root.join("one.wav"), "Needle One", false),
        (nested.join("two.wav"), "Needle Two", true),
        (
            directory.path().join("音楽室/three.wav"),
            "Needle Outside",
            false,
        ),
    ] {
        connection
            .execute(
                "INSERT INTO tracks(path,raw_title,missing) VALUES(?1,?2,?3)",
                rusqlite::params![path.to_str().unwrap(), title, missing],
            )
            .unwrap();
    }
    drop(connection);
    let config_path = directory.path().join("config.toml");
    Config {
        library_roots: vec![root.clone(), nested.clone(), empty.clone()],
        media_read_buffer_mb: 1,
        ..Config::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = Runtime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(&owner, runtime.handle.clone(), app.clone()).unwrap();
    assert!(SocketOwner::acquire(directory.path()).is_err());
    let mut session = client_session(directory.path()).unwrap();
    let count_query = Query::LibraryRootCounts {
        roots: vec![root.clone(), nested.clone(), empty],
    };
    let first = runtime.handle.query(count_query.clone());
    let remote = session.query(&count_query).unwrap();
    assert_eq!(remote.revisions, first.revisions);
    let ViewResponse::LibraryRootCounts(counts) = remote.result.unwrap() else {
        panic!("expected root counts")
    };
    assert_eq!(
        counts.iter().map(|root| root.total).collect::<Vec<_>>(),
        [2, 1, 0]
    );
    let page_query = Query::LibraryRootPage {
        path: root,
        query: Some("needle".into()),
        favorite: None,
        missing: Some(false),
        sort: LibrarySort::Album,
        offset: 0,
        limit: 1,
    };
    let direct = runtime.handle.query(page_query.clone());
    let remote = session.query(&page_query).unwrap();
    assert_eq!(
        serde_json::to_value(&remote).unwrap(),
        serde_json::to_value(&direct).unwrap()
    );
    let ViewResponse::LibraryPage(page) = remote.result.unwrap() else {
        panic!("expected library page")
    };
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].title, "Needle One");
    let ack = session
        .request_ack(&Command::SetFavorite {
            track_ids: vec![page.rows[0].id],
            favorite: true,
        })
        .unwrap();
    assert!(ack.ok);
    let updated = session.query(&count_query).unwrap();
    assert_ne!(updated.revisions.library, first.revisions.library);
    assert_eq!(updated.revisions.structure, first.revisions.structure);
    let ViewResponse::LibraryRootCounts(counts) = updated.result.unwrap() else {
        panic!("expected root counts")
    };
    assert_eq!(
        counts.iter().map(|root| root.total).collect::<Vec<_>>(),
        [2, 1, 0]
    );
    assert!(matches!(
        session
            .query(&Query::LibraryRootCounts {
                roots: vec![nested; MAX_QUERY_ROWS + 1]
            })
            .unwrap()
            .result,
        Err(QueryError::Message(_))
    ));
    drop(session);
    drop(server);
    // Stopping transport does not release the runtime's instance acquisition.
    assert!(SocketOwner::acquire(directory.path()).is_err());
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
    drop(owner);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn playlist_reorder_scoped_revisions_cross_ipc_without_catalog_invalidation() {
    use crate::{
        config::Config,
        core::Runtime,
        response::{PlaylistEntryRevision, QueryError, ViewResponse},
        store::Store,
    };
    let directory = socket_test_directory();
    let database = directory.path().join("library.db");
    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_title) VALUES (1,'one.wav','One'),(2,'two.wav','Two');
         INSERT INTO playlists(id,name) VALUES (10,'one'),(20,'two');
         INSERT INTO playlist_entries(id,playlist_id,track_id,position)
         VALUES (101,10,1,0),(102,10,2,1),(201,20,1,0),(202,20,2,1);",
        )
        .unwrap();
    drop(connection);
    let config_path = directory.path().join("config.toml");
    Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Config::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = Runtime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(&owner, runtime.handle.clone(), app.clone()).unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let summaries = Query::PlaylistSummaries {
        offset: 0,
        limit: 2,
    };
    let catalog = session.query(&summaries).unwrap();
    let batch = |playlist_id, expected| Query::PlaylistEntryBatch {
        playlist_id,
        after: None,
        limit: 1,
        expected,
    };
    let first = session.query(&batch(10, Some(catalog.revisions))).unwrap();
    let other = session.query(&batch(20, Some(catalog.revisions))).unwrap();
    let sections = StateSections {
        library: true,
        ..StateSections::default()
    };
    let before = session.get_state(sections).unwrap();
    assert!(
        session
            .request_ack(&Command::MovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                before_entry_id: None,
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .ok
    );
    let after = session.get_state(sections).unwrap();
    assert_ne!(before.revisions.library, after.revisions.library);
    let library = after.library.unwrap();
    assert_eq!(library.playlist_revision, catalog.revisions.playlist);
    assert_eq!(library.revision, catalog.revisions.library);
    let moved_token = first
        .revisions
        .playlist_entries
        .unwrap()
        .revision
        .wrapping_add(1);
    assert_eq!(
        library.playlist_entry_revisions.get(&10),
        Some(&moved_token)
    );
    assert_eq!(library.playlist_entry_revisions.len(), 1);
    assert_eq!(
        serde_json::to_value(session.query(&summaries).unwrap()).unwrap(),
        serde_json::to_value(catalog).unwrap()
    );
    assert_eq!(
        session
            .query(&batch(10, Some(first.revisions)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .query(&batch(20, Some(other.revisions)))
            .unwrap()
            .result
            .is_ok()
    );
    let selected = session
        .query(&Query::PlaylistEntry { entry_id: 101 })
        .unwrap();
    assert_eq!(
        selected.revisions.playlist_entries,
        Some(PlaylistEntryRevision {
            playlist_id: 10,
            revision: moved_token
        })
    );
    assert!(
        matches!(selected.result, Ok(ViewResponse::PlaylistEntry(Some(entry)))
        if entry.position == 1 && entry.id == 101)
    );
    let locations = session
        .query(&Query::PlaylistEntryLocations {
            playlist_id: 10,
            entry_ids: vec![101, 201, i64::MAX],
        })
        .unwrap();
    assert_eq!(locations.revisions, selected.revisions);
    assert!(
        matches!(locations.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].id == 101 && rows[0].position == 1)
    );
    assert!(
        !session
            .request_ack(&Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .ok
    );
    assert!(
        session
            .request_ack(&Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: moved_token,
            })
            .unwrap()
            .ok
    );
    let removed = session
        .query(&Query::PlaylistEntryLocations {
            playlist_id: 10,
            entry_ids: vec![101],
        })
        .unwrap();
    assert!(
        matches!(removed.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.is_empty())
    );
    assert_ne!(
        removed.revisions.playlist_entries,
        selected.revisions.playlist_entries
    );
    let expected = selected.revisions;
    assert!(
        session
            .request_ack(&Command::RenamePlaylist {
                playlist_id: 10,
                name: "renamed".into()
            })
            .unwrap()
            .ok
    );
    let library = session.get_state(sections).unwrap().library.unwrap();
    assert_ne!(library.playlist_revision, expected.playlist);
    assert!(library.playlist_entry_revisions.is_empty());
    assert_eq!(
        session
            .query(&batch(10, Some(expected)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    drop(session);
    drop(server);
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
    drop(owner);
}

#[test]
fn ack_wire_round_trip_has_explicit_response_type() {
    let ack = Ack {
        ok: false,
        error: Some("failed".into()),
        revision: 42,
        scan_id: Some(u64::MAX),
        relocation_id: Some(42),
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::Ack(ack.clone()), &mut encoded).unwrap();
    assert_eq!(encoded[5], ResponseType::Ack as u8);
    let response = decode_response_frame(&encoded[4..]).unwrap();
    let WireResponse::Ack(decoded) = response else {
        panic!("expected ack")
    };
    assert_eq!(decoded.revision, ack.revision);
    assert_eq!(decoded.error, ack.error);
    assert_eq!(decoded.scan_id, ack.scan_id);
}

#[test]
fn query_wire_response_has_no_state_queue_payload() {
    let response = QueryResponse {
        revisions: crate::response::QueryRevisions::default(),
        result: Err(crate::response::QueryError::Message("query failed".into())),
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
    let response = decode_response_frame(&encoded[4..]).unwrap();
    assert!(matches!(
        response,
        WireResponse::Query(QueryResponse { result: Err(_), .. })
    ));
}

#[test]
fn partial_state_wire_round_trip_omits_unrequested_sections() {
    let response = StateResponse {
        revisions: StateRevisions {
            playback: 1,
            queue: 2,
            library: 3,
            system: 4,
        },
        playback: None,
        queue: None,
        library: Some(crate::model::LibrarySnapshot::default()),
        system: None,
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::State(wire_state(response)), &mut encoded).unwrap();
    let response = decode_response_frame(&encoded[4..]).unwrap();
    let WireResponse::State(state) = response else {
        panic!("expected state")
    };
    let state = state_response(state);
    assert_eq!(state.revisions.library, 3);
    assert!(state.library.is_some());
    assert!(state.playback.is_none());
    assert!(state.queue.is_none());
    assert!(state.system.is_none());
}

#[test]
fn system_config_wire_adapter_preserves_optional_fields_and_library_columns() {
    for (output_device, library_columns) in [
        (
            None,
            crate::config::LibraryColumns {
                duration: false,
                ..Default::default()
            },
        ),
        (
            Some("USB audio output".to_owned()),
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        let config = Arc::new(crate::config::Config {
            output_device,
            library_columns,
            album_artwork_mode: crate::config::AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 12.5,
            album_artwork_direction: crate::config::AlbumArtworkDirection::Counterclockwise,
            ..Default::default()
        });
        let system = crate::model::SystemState {
            config: config.clone(),
            revision: 42,
            ..crate::model::SystemState::default()
        };
        let mut encoded = Vec::new();
        encode_response_frame_into(
            &WireResponse::State(wire_state(StateResponse {
                revisions: StateRevisions::default(),
                playback: None,
                queue: None,
                library: None,
                system: Some(system),
            })),
            &mut encoded,
        )
        .unwrap();
        let response = decode_response_frame(&encoded[4..]).unwrap();
        let WireResponse::State(state) = response else {
            panic!("expected state")
        };
        assert_eq!(
            state_response(state).system.unwrap().config.as_ref(),
            config.as_ref()
        );
    }
}

#[test]
fn request_round_trip_uses_explicit_message_types() {
    let requests = [
        WireRequest::Hello(StateSections::ALL),
        WireRequest::Query(WireQuery::from(Query::LibraryPage {
            query: Some("album".into()),
            favorite: Some(true),
            missing: Some(false),
            sort: crate::model::LibrarySort::Album,
            offset: 7,
            limit: 11,
        })),
        WireRequest::State(StateSections::ALL),
        WireRequest::Command(WireCommand::from(Command::MoveQueue {
            queue_id: 4,
            index: 9,
        })),
        WireRequest::Watch(StateRevisions {
            playback: 1,
            queue: 2,
            library: 3,
            system: 4,
        }),
    ];
    for request in requests {
        let expected_type = match &request {
            WireRequest::Hello(_) => RequestType::Hello,
            WireRequest::State(_) => RequestType::State,
            WireRequest::Query(_) => RequestType::Query,
            WireRequest::Command(_) => RequestType::Command,
            WireRequest::Watch(_) => RequestType::Watch,
        };
        let mut encoded = Vec::new();
        encode_request_frame_into(&request, &mut encoded).unwrap();
        assert_eq!(encoded[5], expected_type as u8);
        let decoded = decode_request_frame(&encoded[4..]).unwrap();
        match decoded {
            WireRequest::Query(WireQuery::LibraryPage { offset, limit, .. }) => {
                assert_eq!((offset, limit), (7, 11))
            }
            WireRequest::State(_) | WireRequest::Hello(_) => {}
            WireRequest::Command(WireCommand::MoveQueue { queue_id, index }) => {
                assert_eq!((queue_id, index), (4, 9))
            }
            WireRequest::Watch(StateRevisions {
                playback,
                queue,
                library,
                system,
            }) => assert_eq!((playback, queue, library, system), (1, 2, 3, 4)),
            _ => panic!("unexpected request variant"),
        }
    }
}

#[test]
fn configure_request_round_trip_preserves_optional_config_field_and_library_columns() {
    for (output_device, library_columns) in [
        (
            None,
            crate::config::LibraryColumns {
                duration: false,
                ..Default::default()
            },
        ),
        (
            Some("USB audio output".to_owned()),
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        let config = crate::config::Config {
            audio_visualizer_grid: output_device.is_some(),
            audio_visualizer_labels: output_device.is_none(),
            output_device,
            library_columns,
            album_artwork_mode: crate::config::AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 12.5,
            album_artwork_direction: crate::config::AlbumArtworkDirection::Counterclockwise,
            ..Default::default()
        };
        let request = WireRequest::Command(WireCommand::from(Command::Configure {
            config: std::sync::Arc::new(config.clone()),
        }));
        let mut encoded = Vec::new();
        encode_request_frame_into(&request, &mut encoded).unwrap();
        let decoded = decode_request_frame(&encoded[4..]).unwrap();
        let WireRequest::Command(command) = decoded else {
            panic!("expected command request")
        };
        let Command::Configure { config: decoded } = command.try_into().unwrap() else {
            panic!("expected configure command")
        };
        assert_eq!(decoded.as_ref(), &config);
    }
}

#[test]
fn edit_track_wire_round_trip_preserves_omitted_and_explicit_fields() {
    for (title, artist, album) in [
        (Some("Title".to_owned()), None, None),
        (None, Some(String::new()), None),
        (None, None, Some("Album".to_owned())),
        (None, None, None),
    ] {
        let request = WireRequest::Command(WireCommand::from(Command::EditTrack {
            track_id: 12,
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            decoder_hint: None,
        }));
        let mut encoded = Vec::new();
        encode_request_frame_into(&request, &mut encoded).unwrap();
        let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected command request");
        };
        let Command::EditTrack {
            track_id,
            title: decoded_title,
            artist: decoded_artist,
            album: decoded_album,
            decoder_hint: None,
        } = command.try_into().unwrap()
        else {
            panic!("expected metadata patch");
        };
        assert_eq!(track_id, 12);
        assert_eq!(
            (decoded_title, decoded_artist, decoded_album),
            (title, artist, album)
        );
    }
}

#[test]
fn request_encoding_rejects_oversized_body_before_completion() {
    let request = WireRequest::Command(WireCommand::from(Command::EditTrack {
        track_id: 1,
        title: Some("x".repeat(MAX_REQUEST)),
        artist: None,
        album: None,
        decoder_hint: None,
    }));
    let mut encoded = Vec::new();
    assert!(encode_request_frame_into(&request, &mut encoded).is_err());
    assert!(encoded.len() <= 4 + MAX_REQUEST);
}

#[test]
fn postcard_frame_headers_and_bodies_preserve_wire_contract() {
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    assert_eq!(encoded, [6, 0, 0, 0, PROTOCOL_VERSION, 0, 1, 1, 1, 1]);
    encode_response_frame_into(&WireResponse::Error("x".into()), &mut encoded).unwrap();
    assert_eq!(encoded, [4, 0, 0, 0, PROTOCOL_VERSION, 255, 1, b'x']);
}

#[tokio::test]
async fn incompatible_versions_are_rejected_before_any_body_decode_including_hello() {
    for version in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
        for message_type in [RequestType::Hello, RequestType::Command] {
            let error = decode_request_frame(&[version, message_type as u8, 0xff])
                .err()
                .unwrap();
            assert!(error.downcast_ref::<postcard::Error>().is_none());
        }
        for message_type in [ResponseType::Hello, ResponseType::Error] {
            let encoded = [3, 0, 0, 0, version, message_type as u8, 0xff];
            let mut stream = encoded.as_slice();
            let mut pending = bytes::BytesMut::new();
            let error = read_response_frame(&mut stream, &mut pending)
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<postcard::Error>().is_none());
        }
    }
}

#[test]
fn truncated_headers_are_rejected_by_both_decoders() {
    for bytes in [&[][..], &[PROTOCOL_VERSION][..]] {
        assert!(decode_request_frame(bytes).is_err());
        assert!(decode_response_frame(bytes).is_err());
    }
}

#[test]
fn unknown_wire_type_is_rejected_before_body_decode() {
    assert!(decode_request_frame(&[PROTOCOL_VERSION, 99]).is_err());
    assert!(decode_response_frame(&[PROTOCOL_VERSION, 99]).is_err());
}

#[test]
fn response_decode_rejects_malicious_postcard_string_length_without_payload() {
    // Raw varints for MAX_RESPONSE + 1 and u64::MAX, independent of the encoder.
    for length in [
        &[0x81, 0x80, 0x80, 0x08][..],
        &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..],
    ] {
        let mut encoded = vec![PROTOCOL_VERSION, ResponseType::Error as u8];
        encoded.extend_from_slice(length);
        assert!(decode_response_frame(&encoded).is_err());
    }
}

#[test]
fn command_decode_rejects_malicious_postcard_collection_length() {
    // Raw varints for MAX_REQUEST + 1 and u64::MAX.
    for length in [
        &[0x81, 0x80, 0x04][..],
        &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..],
    ] {
        let mut encoded = Vec::new();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::Enqueue {
                track_ids: Vec::new(),
            }),
            &mut encoded,
        )
        .unwrap();
        // Replace the empty vector length and append one real element.
        assert_eq!(encoded.pop(), Some(0));
        encoded.extend_from_slice(length);
        encoded.push(2); // Postcard's zigzag encoding of 1i64.
        assert!(decode_request_frame(&encoded[4..]).is_err());
    }
}

#[test]
fn response_decode_rejects_malicious_postcard_collection_length() {
    let response = WireResponse::Query(QueryResponse {
        revisions: crate::response::QueryRevisions::default(),
        result: Ok(crate::response::ViewResponse::PlaylistSummaries(
            crate::model::Page {
                total: 0,
                rows: Vec::new(),
            },
        )),
    });
    let mut encoded = Vec::new();
    encode_response_frame_into(&response, &mut encoded).unwrap();
    assert_eq!(encoded.pop(), Some(0));
    encoded.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    assert!(decode_response_frame(&encoded[4..]).is_err());
}

#[test]
fn request_and_response_decoding_reject_trailing_bytes() {
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    encoded.push(0);
    assert!(decode_request_frame(&encoded[4..]).is_err());
    encode_response_frame_into(
        &WireResponse::Watch(StateRevisions::default()),
        &mut encoded,
    )
    .unwrap();
    encoded.push(0);
    assert!(decode_response_frame(&encoded[4..]).is_err());
}

#[test]
fn truncated_postcard_bodies_and_malformed_varints_are_rejected() {
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    encoded.pop();
    let error = decode_request_frame(&encoded[4..]).err().unwrap();
    assert_eq!(
        error.downcast_ref::<postcard::Error>(),
        Some(&postcard::Error::DeserializeUnexpectedEnd)
    );
    encode_response_frame_into(&WireResponse::Error("hello".into()), &mut encoded).unwrap();
    encoded.pop();
    let error = decode_response_frame(&encoded[4..]).err().unwrap();
    assert_eq!(
        error.downcast_ref::<postcard::Error>(),
        Some(&postcard::Error::DeserializeUnexpectedEnd)
    );
    let mut malformed = vec![PROTOCOL_VERSION, ResponseType::Error as u8];
    malformed.extend([0xff; 20]);
    let error = decode_response_frame(&malformed).err().unwrap();
    assert_eq!(
        error.downcast_ref::<postcard::Error>(),
        Some(&postcard::Error::DeserializeBadVarint)
    );
}

#[test]
fn response_encoding_is_bounded_and_buffer_can_be_reused_after_failure() {
    let mut encoded = Vec::new();
    assert!(
        encode_response_frame_into(&WireResponse::Error("x".repeat(MAX_RESPONSE)), &mut encoded,)
            .is_err()
    );
    assert!(encoded.len() <= 4 + MAX_RESPONSE);
    encode_response_frame_into(&WireResponse::Error("small".into()), &mut encoded).unwrap();
    assert!(matches!(
        decode_response_frame(&encoded[4..]).unwrap(),
        WireResponse::Error(error) if error == "small"
    ));
}

#[tokio::test]
async fn oversized_frame_is_rejected_before_consuming_or_reserving_its_body() {
    for limit in [MAX_REQUEST, MAX_RESPONSE] {
        for length in [(limit + 1) as u32, u32::MAX] {
            let mut encoded = length.to_le_bytes().to_vec();
            encoded.push(0xaa);
            let mut stream = encoded.as_slice();
            let mut pending = bytes::BytesMut::new();
            assert!(read_frame(&mut stream, limit, &mut pending).await.is_err());
            assert_eq!(stream, &[0xaa]);
            assert_eq!(pending.len(), 4);
            assert!(pending.capacity() < limit);
        }
    }
}

#[test]
fn direct_decoders_enforce_frame_limits_before_body_decode() {
    let mut encoded = vec![0xff; MAX_REQUEST + 1];
    encoded[0] = PROTOCOL_VERSION;
    encoded[1] = RequestType::Command as u8;
    let error = decode_request_frame(&encoded).err().unwrap();
    assert!(error.downcast_ref::<postcard::Error>().is_none());
    encoded.resize(MAX_RESPONSE + 1, 0xff);
    encoded[1] = ResponseType::Error as u8;
    let error = decode_response_frame(&encoded).err().unwrap();
    assert!(error.downcast_ref::<postcard::Error>().is_none());
}

#[tokio::test]
async fn response_frame_boundary_preserves_the_next_message() {
    let mut encoded = Vec::new();
    let mut second = Vec::new();
    encode_response_frame_into(&WireResponse::Error("first".into()), &mut encoded).unwrap();
    encode_response_frame_into(&WireResponse::Error("second".into()), &mut second).unwrap();
    encoded.extend_from_slice(&second);
    for buffered in [false, true] {
        let mut pending = bytes::BytesMut::new();
        let mut stream = encoded.as_slice();
        if buffered {
            pending.extend_from_slice(stream);
            stream = &[];
        }
        for expected in ["first", "second"] {
            assert!(matches!(
                read_response_frame(&mut stream, &mut pending).await.unwrap(),
                WireResponse::Error(error) if error == expected
            ));
        }
        assert!(stream.is_empty());
        assert!(pending.is_empty());
    }
}

#[tokio::test]
async fn segmented_frame_reads_preserve_payload_and_leave_the_next_frame_unread() {
    use tokio::io::AsyncReadExt;

    let mut encoded = Vec::new();
    let mut second = Vec::new();
    encode_response_frame_into(
        &WireResponse::Error("segmented response".repeat(1024)),
        &mut encoded,
    )
    .unwrap();
    encode_response_frame_into(&WireResponse::Error("next".into()), &mut second).unwrap();
    let first_length = encoded.len();
    encoded.extend_from_slice(&second);

    for split in [1, 2, 3, 4, 5, 8191, 8192, 8193, first_length - 1] {
        let mut stream = (&encoded[..split]).chain(&encoded[split..]);
        let mut pending = bytes::BytesMut::new();
        let frame = read_frame(&mut stream, MAX_RESPONSE, &mut pending)
            .await
            .unwrap();
        assert_eq!(frame.as_ref(), &encoded[4..first_length]);
        assert!(pending.is_empty());
        let mut unread = Vec::new();
        stream.read_to_end(&mut unread).await.unwrap();
        assert_eq!(unread, second);
    }
}

#[tokio::test]
async fn cancelled_frame_reads_resume_from_every_partial_prefix_and_body() {
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    use tokio::io::AsyncWriteExt;

    let mut encoded = Vec::new();
    let mut second = Vec::new();
    encode_response_frame_into(&WireResponse::Error("resumed".into()), &mut encoded).unwrap();
    encode_response_frame_into(&WireResponse::Error("next".into()), &mut second).unwrap();

    for split in 1..encoded.len() {
        let (mut writer, mut reader) = tokio::io::duplex(encoded.len() + second.len());
        let mut pending = bytes::BytesMut::new();
        writer.write_all(&encoded[..split]).await.unwrap();
        {
            let mut reading = std::pin::pin!(read_response_frame(&mut reader, &mut pending));
            let mut context = Context::from_waker(Waker::noop());
            assert!(reading.as_mut().poll(&mut context).is_pending());
        }
        assert_eq!(pending.as_ref(), &encoded[..split]);

        writer.write_all(&encoded[split..]).await.unwrap();
        writer.write_all(&second).await.unwrap();
        writer.shutdown().await.unwrap();
        assert!(matches!(
            read_response_frame(&mut reader, &mut pending).await.unwrap(),
            WireResponse::Error(error) if error == "resumed"
        ));
        assert!(pending.is_empty());
        assert!(matches!(
            read_response_frame(&mut reader, &mut pending).await.unwrap(),
            WireResponse::Error(error) if error == "next"
        ));
        assert!(pending.is_empty());
    }
}

#[test]
fn maximum_sized_request_and_response_bodies_round_trip() {
    // Two header bytes, one command discriminant, four string-length bytes.
    let name = "x".repeat(MAX_REQUEST - 7);
    let mut encoded = Vec::new();
    encode_request_frame_into(
        &WireRequest::Command(WireCommand::CreatePlaylist { name }),
        &mut encoded,
    )
    .unwrap();
    assert_eq!(encoded.len(), 4 + MAX_REQUEST);
    assert_eq!(
        u32::from_le_bytes(encoded[..4].try_into().unwrap()) as usize,
        MAX_REQUEST
    );
    let WireRequest::Command(WireCommand::CreatePlaylist { name }) =
        decode_request_frame(&encoded[4..]).unwrap()
    else {
        panic!("expected create playlist request")
    };
    assert_eq!(name.len(), MAX_REQUEST - 7);

    // Two header bytes and four string-length bytes.
    encode_response_frame_into(
        &WireResponse::Error("x".repeat(MAX_RESPONSE - 6)),
        &mut encoded,
    )
    .unwrap();
    assert_eq!(encoded.len(), 4 + MAX_RESPONSE);
    assert_eq!(
        u32::from_le_bytes(encoded[..4].try_into().unwrap()) as usize,
        MAX_RESPONSE
    );
    let WireResponse::Error(message) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected error response")
    };
    assert_eq!(message.len(), MAX_RESPONSE - 6);
}

pub(crate) fn socket_test_directory() -> tempfile::TempDir {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    std::fs::create_dir_all(&cache).unwrap();
    tempfile::tempdir_in(cache).unwrap()
}

fn read_test_request(stream: &mut std::os::unix::net::UnixStream) -> WireRequest {
    use std::io::Read;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let length = u32::from_le_bytes(prefix) as usize;
    assert!(length <= MAX_REQUEST);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    decode_request_frame(&body).unwrap()
}

fn write_test_response(stream: &mut std::os::unix::net::UnixStream, response: WireResponse) {
    use std::io::Write;
    let mut frame = Vec::new();
    encode_response_frame_into(&response, &mut frame).unwrap();
    stream.write_all(&frame).unwrap();
}

pub(crate) fn complete_test_handshake(stream: &mut std::os::unix::net::UnixStream) {
    complete_test_handshake_for_instance(stream, 1);
}

pub(crate) fn complete_test_handshake_for_instance(
    stream: &mut std::os::unix::net::UnixStream,
    instance_id: u32,
) {
    let timeout = Some(std::time::Duration::from_secs(5));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    assert!(matches!(read_test_request(stream), WireRequest::Hello(_)));
    write_test_response(
        stream,
        WireResponse::Hello(HelloResponse {
            instance_id: instance_id.to_le_bytes(),
            state: wire_state(StateResponse {
                revisions: StateRevisions::default(),
                playback: None,
                queue: None,
                library: None,
                system: None,
            }),
        }),
    );
}

pub(crate) fn read_test_query(stream: &mut std::os::unix::net::UnixStream) -> Query {
    let WireRequest::Query(query) = read_test_request(stream) else {
        panic!("expected query")
    };
    Query::try_from(query).unwrap()
}

pub(crate) fn reply_test_query(
    stream: &mut std::os::unix::net::UnixStream,
    response: QueryResponse,
) {
    write_test_response(stream, WireResponse::Query(response));
}

pub(crate) fn read_test_command(stream: &mut std::os::unix::net::UnixStream) -> Command {
    let WireRequest::Command(command) = read_test_request(stream) else {
        panic!("expected command")
    };
    Command::try_from(command).unwrap()
}

pub(crate) fn reply_test_ack(stream: &mut std::os::unix::net::UnixStream, ack: Ack) {
    write_test_response(stream, WireResponse::Ack(ack));
}

fn playlist_test_response(total: usize) -> QueryResponse {
    QueryResponse {
        revisions: crate::response::QueryRevisions::default(),
        result: Ok(crate::response::ViewResponse::PlaylistSummaries(
            crate::model::Page {
                total,
                rows: Vec::new(),
            },
        )),
    }
}

#[test]
fn client_session_reuses_connection_after_command_and_request_rejections() {
    use std::os::unix::net::UnixListener;

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(matches!(
            read_test_query(&mut stream),
            Query::PlaylistSummaries { offset: 0, .. }
        ));
        reply_test_query(&mut stream, playlist_test_response(40));
        assert!(matches!(
            read_test_command(&mut stream),
            Command::CreatePlaylist { .. }
        ));
        reply_test_ack(
            &mut stream,
            Ack {
                ok: false,
                error: Some("rejected".into()),
                revision: 3,
                scan_id: None,
                relocation_id: None,
            },
        );
        assert!(matches!(
            read_test_query(&mut stream),
            Query::PlaylistSummaries { offset: 20, .. }
        ));
        write_test_response(
            &mut stream,
            WireResponse::Error("request unavailable".into()),
        );
        assert!(matches!(
            read_test_query(&mut stream),
            Query::PlaylistSummaries { offset: 20, .. }
        ));
        reply_test_query(&mut stream, playlist_test_response(7));
    });
    let mut session = client_session(directory.path()).unwrap();
    session.bind_instance(1).unwrap();
    let query = Query::PlaylistSummaries {
        offset: 0,
        limit: 20,
    };
    let response = session.query(&query).unwrap();
    assert!(
        matches!(response.result, Ok(crate::response::ViewResponse::PlaylistSummaries(page)) if page.total == 40)
    );
    let ack = session
        .request_ack(&Command::CreatePlaylist { name: "new".into() })
        .unwrap();
    assert!(!ack.ok);
    assert_eq!(ack.revision, 3);
    let query = Query::PlaylistSummaries {
        offset: 20,
        limit: 20,
    };
    assert!(session.query(&query).is_err());
    let response = session.query(&query).unwrap();
    assert!(
        matches!(response.result, Ok(crate::response::ViewResponse::PlaylistSummaries(page)) if page.total == 7)
    );
    server.join().unwrap();
}

#[test]
fn client_session_discards_partial_failed_response_before_next_explicit_request() {
    use std::{io::Write, os::unix::net::UnixListener};

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
        );
        let mut frame = Vec::new();
        encode_response_frame_into(
            &WireResponse::Ack(Ack {
                ok: true,
                error: None,
                revision: 1,
                scan_id: None,
                relocation_id: None,
            }),
            &mut frame,
        )
        .unwrap();
        stream.write_all(&frame[..5]).unwrap();
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        // The first mutation may have executed. The new request, not a replay,
        // must be the next operation after the replacement handshake.
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "second")
        );
        reply_test_ack(
            &mut stream,
            Ack {
                ok: true,
                error: None,
                revision: 2,
                scan_id: None,
                relocation_id: None,
            },
        );
    });
    let mut session = client_session(directory.path()).unwrap();
    session.bind_instance(1).unwrap();
    assert!(
        session
            .request_ack(&Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    let ack = session
        .request_ack(&Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap();
    assert!(ack.ok);
    assert_eq!(ack.revision, 2);
    server.join().unwrap();
}

#[test]
fn cancelling_client_session_closes_pending_watch_with_partial_response() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        sync::atomic::Ordering,
    };

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let (cancelled, notify) = session.cancellation();
    let client = std::thread::spawn(move || {
        session.get_state(StateSections::default()).unwrap();
        session
            .bind_instance(session.instance_id().unwrap())
            .unwrap();
        session.watch_until(StateRevisions::default())
    });
    let mut stream = listener.accept().unwrap().0;
    complete_test_handshake(&mut stream);
    assert!(matches!(
        read_test_request(&mut stream),
        WireRequest::Watch(_)
    ));
    // Cancellation discards a partial response, not just an idle watch socket.
    stream.write_all(&[10, 0, 0, 0, PROTOCOL_VERSION]).unwrap();
    cancelled.store(true, Ordering::Release);
    notify.notify_waiters();
    assert!(client.join().unwrap().unwrap().is_none());
    let closed = stream.read(&mut [0]);
    assert!(
        matches!(closed, Ok(0))
            || matches!(closed, Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset)
    );
}

#[test]
fn peer_verification_checks_kernel_credentials_on_both_ends() {
    let directory = socket_test_directory();
    let address = super::endpoint::address(directory.path()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind_addr(&address).unwrap();
    let client = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
    let accepted = listener.accept().unwrap().0;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    for stream in [client, accepted] {
        stream.set_nonblocking(true).unwrap();
        let stream = tokio::net::UnixStream::from_std(stream).unwrap();
        assert_eq!(stream.peer_cred().unwrap().uid(), uid);
        assert_eq!(stream.peer_cred().unwrap().gid(), gid);
        assert!(super::endpoint::verify_peer(&stream).is_ok());
    }
}

#[test]
fn pinned_session_rejects_initial_replacement_before_commands_queries_or_state() {
    use std::{io::Read, os::unix::net::UnixListener};
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "old baseline sent an operation to B"
            );
        }
    });
    let mut session = client_session(directory.path()).unwrap();
    session.bind_instance(1).unwrap();
    let error = session
        .request_ack(&Command::CreatePlaylist {
            name: "A-only".into(),
        })
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    let error = session
        .query(&Query::PlaylistSummaries {
            offset: 0,
            limit: 10,
        })
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    let error = session.get_state(StateSections::ALL).unwrap_err();
    assert!(error.is::<InstanceChanged>());
    assert_eq!(session.instance_id(), None);
    server.join().unwrap();
}

#[test]
fn pinned_session_rejects_reconnected_instance_without_replaying_failed_mutation() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
        );
        stream.write_all(&[10, 0, 0, 0, PROTOCOL_VERSION]).unwrap();
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake_for_instance(&mut stream, 2);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let mut session = client_session(directory.path()).unwrap();
    session.bind_instance(1).unwrap();
    assert!(
        session
            .request_ack(&Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    let error = session
        .request_ack(&Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    server.join().unwrap();
}

#[test]
fn independent_cli_session_can_explicitly_reconnect_to_a_new_instance() {
    use std::os::unix::net::UnixListener;
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&super::endpoint::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        read_test_command(&mut stream);
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake_for_instance(&mut stream, 2);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "second")
        );
        reply_test_ack(
            &mut stream,
            Ack {
                ok: true,
                error: None,
                revision: 2,
                scan_id: None,
                relocation_id: None,
            },
        );
    });
    let mut session = client_session(directory.path()).unwrap();
    assert!(
        session
            .request_ack(&Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    assert_eq!(session.instance_id(), None);
    assert!(
        session
            .request_ack(&Command::CreatePlaylist {
                name: "second".into()
            })
            .unwrap()
            .ok
    );
    assert_eq!(session.instance_id(), Some(2));
    server.join().unwrap();
}

#[test]
fn instance_acquisition_creates_directory_without_socket_or_lock_files() {
    let directory = socket_test_directory();
    let data_dir = directory.path().join("data");
    let owner = SocketOwner::acquire(&data_dir).unwrap();
    assert!(data_dir.is_dir());
    assert_eq!(std::fs::read_dir(&data_dir).unwrap().count(), 0);
    assert!(SocketOwner::acquire(&data_dir).is_err());
    let address = super::endpoint::address(&data_dir).unwrap();
    assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    drop(owner);
    assert_eq!(std::fs::read_dir(&data_dir).unwrap().count(), 0);
    assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_err());
    assert!(SocketOwner::acquire(&data_dir).is_ok());
}

#[test]
fn directory_aliases_share_instance_ownership_and_transport() {
    let directory = socket_test_directory();
    let data_dir = directory.path().join("data");
    let owner = SocketOwner::acquire(&data_dir).unwrap();
    let symlink = directory.path().join("alias");
    std::os::unix::fs::symlink(&data_dir, &symlink).unwrap();
    let current_dir = std::env::current_dir().unwrap();
    let relative = data_dir.strip_prefix(&current_dir).unwrap();
    for alias in [symlink.as_path(), relative] {
        assert!(SocketOwner::acquire(alias).is_err());
        let address = super::endpoint::address(alias).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    }
    drop(owner);
    let owner = SocketOwner::acquire(&symlink).unwrap();
    assert!(SocketOwner::acquire(&data_dir).is_err());
    drop(owner);
    assert!(SocketOwner::acquire(relative).is_ok());
}

#[test]
fn distinct_data_directories_acquire_independent_instances() {
    let directory = socket_test_directory();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let first_owner = SocketOwner::acquire(&first).unwrap();
    let second_owner = SocketOwner::acquire(&second).unwrap();
    for data_dir in [&first, &second] {
        assert!(SocketOwner::acquire(data_dir).is_err());
        let address = super::endpoint::address(data_dir).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    }
    drop(first_owner);
    let first_owner = SocketOwner::acquire(&first).unwrap();
    assert!(SocketOwner::acquire(&second).is_err());
    drop(second_owner);
    assert!(SocketOwner::acquire(&second).is_ok());
    drop(first_owner);
}

#[test]
fn simultaneous_starters_acquire_one_real_listener() {
    let directory = socket_test_directory();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    SocketOwner::acquire(directory.path())
                })
            })
            .collect();
        let owners: Vec<_> = handles
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().ok())
            .collect();
        assert_eq!(owners.len(), 1);
        let address = super::endpoint::address(directory.path()).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    });
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn album_queries_and_commands_round_trip_exact_keys() {
    let key = crate::model::AlbumKey {
        directory: "/音楽/album".into(),
        title: String::new(),
    };
    for query in [
        Query::Albums {
            query: Some("artist 音楽".into()),
            sort: crate::model::AlbumSort::Artist,
            offset: 24,
            limit: 24,
        },
        Query::Albums {
            query: None,
            sort: crate::model::AlbumSort::Title,
            offset: 0,
            limit: 24,
        },
        Query::AlbumTracks {
            key: key.clone(),
            offset: 256,
            limit: 256,
        },
    ] {
        let expected = serde_json::to_value(&query).unwrap();
        let mut encoded = Vec::new();
        encode_request_frame_into(&WireRequest::Query(query.into()), &mut encoded).unwrap();
        let WireRequest::Query(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("wrong request");
        };
        assert_eq!(
            serde_json::to_value(Query::try_from(decoded).unwrap()).unwrap(),
            expected
        );
    }
    for command in [
        Command::PlayAlbum { key: key.clone() },
        Command::EnqueueAlbum { key },
    ] {
        let expected = serde_json::to_value(&command).unwrap();
        let mut encoded = Vec::new();
        encode_request_frame_into(&WireRequest::Command(command.into()), &mut encoded).unwrap();
        let WireRequest::Command(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("wrong request");
        };
        assert_eq!(
            serde_json::to_value(Command::try_from(decoded).unwrap()).unwrap(),
            expected
        );
    }
}

#[test]
fn queue_queries_and_full_queue_playlist_commands_round_trip() {
    use crate::model::QueueCursor;
    for query in [
        Query::QueuePage {
            offset: 199_936,
            limit: 256,
        },
        Query::QueueBatch {
            after: Some(QueueCursor {
                position: 255,
                id: 1000,
            }),
            limit: 256,
            expected: Some(crate::response::QueryRevisions {
                queue: 42,
                ..Default::default()
            }),
        },
        Query::QueueEntries {
            queue_ids: vec![1, 3, 3],
        },
    ] {
        let expected = serde_json::to_value(&query).unwrap();
        let mut encoded = Vec::new();
        encode_request_frame_into(&WireRequest::Query(query.into()), &mut encoded).unwrap();
        let WireRequest::Query(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("wrong response");
        };
        assert_eq!(
            serde_json::to_value(Query::try_from(decoded).unwrap()).unwrap(),
            expected
        );
    }
    let command = Command::CreatePlaylistFromQueue {
        name: "All occurrences".into(),
    };
    let expected = serde_json::to_value(&command).unwrap();
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Command(command.into()), &mut encoded).unwrap();
    let WireRequest::Command(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("wrong response");
    };
    assert_eq!(
        serde_json::to_value(Command::try_from(decoded).unwrap()).unwrap(),
        expected
    );
}

#[test]
fn large_explicit_occurrence_selections_fit_bounded_request_frames() {
    let mut encoded = Vec::new();
    encode_request_frame_into(
        &WireRequest::Command(
            Command::MoveQueueEntries {
                queue_ids: (1..=200_000).collect(),
                index: 0,
            }
            .into(),
        ),
        &mut encoded,
    )
    .unwrap();
    assert!(encoded.len() < MAX_REQUEST);
    let WireRequest::Command(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("wrong response");
    };
    let Command::MoveQueueEntries { queue_ids, .. } = Command::try_from(decoded).unwrap() else {
        panic!("wrong command");
    };
    assert_eq!(queue_ids.len(), 200_000);
}
