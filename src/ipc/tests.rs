use super::*;
use crate::app::AppHandle;
use crate::config::ConfigPersistence;
use rivu_client::{CommandFailure, InstanceChanged, client_session};
use rivu_protocol::{AppCommand, protocol::*, transport::*};
use rivu_protocol::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use std::{path::PathBuf, sync::Arc};

#[test]
fn pending_query_preserves_pipelined_responses_and_disconnected_clients_release_slots() {
    use std::io::{Read, Write};
    use std::time::Duration;

    fn read_response(stream: &mut std::os::unix::net::UnixStream) -> WireResponse {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= MAX_RESPONSE);
        let mut frame = vec![0; length];
        stream.read_exact(&mut frame).unwrap();
        decode_response_frame(&frame).unwrap()
    }

    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let mut runtime = crate::core::Runtime::start(directory.path(), &config_path).unwrap();
    let (handle, queries) = crate::core::CoreHandle::test_query_port();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(&owner, handle.clone(), app).unwrap();
    let address = rivu_protocol::protocol::address(directory.path()).unwrap();
    let connect = || {
        let mut stream = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut frame = Vec::new();
        encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut frame).unwrap();
        stream.write_all(&frame).unwrap();
        assert!(matches!(read_response(&mut stream), WireResponse::Hello(_)));
        stream
    };
    let receive_query =
        || async_io::block_on(timeout(Duration::from_secs(2), queries.receive())).unwrap();
    let response = || QueryResponse {
        revisions: Default::default(),
        result: Ok(rivu_protocol::response::ViewResponse::Track(None)),
    };
    let mut stream = connect();
    let mut frame = Vec::new();
    encode_request_frame_into(
        &WireRequest::Query(WireQuery::from(Query::Track { track_id: 1 })),
        &mut frame,
    )
    .unwrap();
    stream.write_all(&frame).unwrap();
    let reply = receive_query();
    encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut frame).unwrap();
    stream.write_all(&frame).unwrap();
    encode_request_frame_into(
        &WireRequest::Query(WireQuery::from(Query::Track { track_id: 2 })),
        &mut frame,
    )
    .unwrap();
    stream.write_all(&frame[..1]).unwrap();
    stream.set_nonblocking(true).unwrap();
    assert_eq!(
        stream.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    stream.set_nonblocking(false).unwrap();
    assert!(!reply.is_cancelled());
    reply.respond(response());
    assert!(matches!(read_response(&mut stream), WireResponse::Query(_)));
    assert!(matches!(read_response(&mut stream), WireResponse::State(_)));
    stream.write_all(&frame[1..]).unwrap();
    receive_query().respond(response());
    assert!(matches!(read_response(&mut stream), WireResponse::Query(_)));
    drop(stream);

    // More than MAX_CLIENTS abandoned reads must still admit another Hello
    // without waiting for the controlled query consumer to send any response.
    for _ in 0..=MAX_CLIENTS {
        let mut stream = connect();
        encode_request_frame_into(
            &WireRequest::Query(WireQuery::from(Query::LibraryStats)),
            &mut frame,
        )
        .unwrap();
        stream.write_all(&frame).unwrap();
        let mut reply = receive_query();
        drop(stream);
        async_io::block_on(timeout(Duration::from_secs(2), reply.cancelled())).unwrap();
    }
    drop(connect());
    assert_eq!(handle.test_revision_subscriptions(), (0, 0));
    drop(server);
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
}

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
    client.query(Query::ScanTask { id: 0 }).unwrap();
    for target in [
        rivu_protocol::model::TaskTarget::Scan { id: u64::MAX },
        rivu_protocol::model::TaskTarget::Relocation { id: u64::MAX },
        rivu_protocol::model::TaskTarget::Processing { id: u64::MAX },
    ] {
        let instance = client.instance_id();
        let error = client.wait_task(target).unwrap_err();
        assert!(
            error
                .downcast_ref::<rivu_protocol::response::QueryError>()
                .is_some()
        );
        assert_eq!(client.instance_id(), instance);
    }
    let before = client.get_state(StateSections::ALL).unwrap();
    let rejected = client.request_ack(AppCommand::ShowWindow).unwrap();
    assert!(!rejected.is_success());
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
    let ack = client.request_ack(AppCommand::Shutdown).unwrap();
    assert!(ack.is_success());
    runtime.join().unwrap();
    assert!(runtime.handle.is_shutting_down());
    assert!(ack.revision <= runtime.handle.publication_revision());
    drop(server);
    assert!(SocketOwner::acquire(directory.path()).is_err());
    drop(owner);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn malformed_request_during_watch_receives_invalid_request_response() {
    use std::io::{Read, Write};

    fn read_response(stream: &mut std::os::unix::net::UnixStream) -> WireResponse {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= MAX_RESPONSE);
        let mut frame = vec![0; length];
        stream.read_exact(&mut frame).unwrap();
        decode_response_frame(&frame).unwrap()
    }

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
    let mut control = client_session(directory.path()).unwrap();
    control.query(Query::ScanTask { id: 0 }).unwrap();
    let address = rivu_protocol::protocol::address(directory.path()).unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
    let timeout = Some(std::time::Duration::from_secs(5));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    stream.write_all(&encoded).unwrap();
    assert!(matches!(read_response(&mut stream), WireResponse::Hello(_)));

    encode_request_frame_into(
        &WireRequest::Watch(runtime.handle.state_revisions()),
        &mut encoded,
    )
    .unwrap();
    // A complete frame with an unknown request type is a decoding failure, not
    // a framing failure: Watch must hand it back to the normal error responder.
    encoded.extend_from_slice(&[2, 0, 0, 0, PROTOCOL_VERSION, 99]);
    stream.write_all(&encoded).unwrap();
    let response = match read_response(&mut stream) {
        // Startup publication may race the Watch baseline.
        WireResponse::Watch(_) => read_response(&mut stream),
        response => response,
    };
    assert!(matches!(
        response,
        WireResponse::Error(error) if error.starts_with("Invalid IPC request:")
    ));
    assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
    drop(stream);
    assert!(
        control
            .request_ack(AppCommand::Shutdown)
            .unwrap()
            .is_success()
    );
    runtime.join().unwrap();
    drop(server);
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
fn other_tracks_requests_round_trip_boundaries() {
    use rivu_protocol::model::LibrarySort;
    let mut encoded = Vec::new();
    for query in [None, Some(String::new()), Some("Été_100%".into())] {
        for favorite in [None, Some(false), Some(true)] {
            for missing in [None, Some(false), Some(true)] {
                for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
                    for offset in [0, usize::MAX] {
                        for expected in [
                            Query::OtherTracksPage {
                                query: query.clone(),
                                favorite,
                                missing,
                                sort,
                                offset,
                                limit: usize::MAX,
                            },
                            Query::OtherTracksLocate {
                                track_id: i64::MAX,
                                query: query.clone(),
                                favorite,
                                missing,
                                sort,
                            },
                            Query::OtherTracksCount,
                        ] {
                            encode_request_frame_into(
                                &WireRequest::Query(WireQuery::from(expected.clone())),
                                &mut encoded,
                            )
                            .unwrap();
                            let WireRequest::Query(wire) =
                                decode_request_frame(&encoded[4..]).unwrap()
                            else {
                                panic!("expected query")
                            };
                            assert_eq!(Query::try_from(wire).unwrap(), expected);
                        }
                    }
                }
            }
        }
    }
    for other_tracks in [false, true] {
        for index in [None, Some(0), Some(usize::MAX)] {
            for expected_revision in [None, Some(0), Some(u64::MAX)] {
                for command in [
                    Command::CheckOtherTracks {
                        force: other_tracks,
                    },
                    Command::QueueExternalPaths {
                        paths: vec!["/音楽/track.wav".into(), "/music".into()],
                        index,
                        expected_revision,
                    },
                    Command::QueueExternalPaths {
                        paths: vec![],
                        index,
                        expected_revision,
                    },
                    Command::EnqueueSources {
                        directories: vec![],
                        track_ids: vec![],
                        other_tracks,
                    },
                    Command::InsertQueueSources {
                        directories: vec!["/music".into()],
                        track_ids: vec![1, i64::MAX],
                        other_tracks,
                        index: index.unwrap_or(0),
                        expected_revision: expected_revision.unwrap_or(0),
                    },
                    Command::AddPlaylistSources {
                        playlist_id: i64::MAX,
                        directories: vec![],
                        track_ids: vec![],
                        other_tracks,
                    },
                    Command::InsertPlaylistSources {
                        playlist_id: i64::MAX,
                        directories: vec![],
                        track_ids: vec![],
                        other_tracks,
                        index: index.unwrap_or(0),
                        expected_revision: expected_revision.unwrap_or(0),
                    },
                ] {
                    let expected = serde_json::to_value(&command).unwrap();
                    encode_request_frame_into(
                        &WireRequest::Command(WireCommand::from(command)),
                        &mut encoded,
                    )
                    .unwrap();
                    let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap()
                    else {
                        panic!("expected command")
                    };
                    assert_eq!(
                        serde_json::to_value(Command::try_from(wire).unwrap()).unwrap(),
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn other_tracks_wire_usize_boundaries_are_checked() {
    use rivu_protocol::model::LibrarySort;
    for (offset, limit) in [(u64::MAX, 0), (0, u64::MAX)] {
        let decoded = Query::try_from(WireQuery::OtherTracksPage {
            query: None,
            favorite: None,
            missing: None,
            sort: LibrarySort::Id,
            offset,
            limit,
        });
        assert_eq!(decoded.is_ok(), usize::try_from(u64::MAX).is_ok());
    }
    let decoded = Command::try_from(WireCommand::QueueExternalPaths {
        paths: vec![],
        index: Some(u64::MAX),
        expected_revision: Some(u64::MAX),
    });
    assert_eq!(decoded.is_ok(), usize::try_from(u64::MAX).is_ok());
}

#[test]
fn optional_search_queries_round_trip() {
    let mut encoded = Vec::new();
    for filter in [None, Some(""), Some("  MiXeD café / \"quoted\" %_  ")] {
        let query = filter.map(str::to_owned);
        for expected in [
            Query::AlbumTracks {
                key: rivu_protocol::model::AlbumKey {
                    directory: "/music".into(),
                    title: "Album".into(),
                },
                query: query.clone(),
                offset: 2,
                limit: 3,
            },
            Query::PlaylistSummaries {
                query: query.clone(),
                offset: 2,
                limit: 3,
            },
            Query::PlaylistEntries {
                query: query.clone(),
                playlist_id: 10,
                offset: 2,
                limit: 3,
            },
            Query::PlaylistEntryLocations {
                query: query.clone(),
                playlist_id: 10,
                entry_ids: vec![42, 43],
            },
            Query::PlaylistSummaryBatch {
                query: query.clone(),
                after: Some(10),
                limit: 3,
                expected: None,
            },
            Query::PlaylistEntryBatch {
                query: query.clone(),
                playlist_id: 10,
                after: None,
                limit: 3,
                expected: None,
            },
            Query::QueuePage {
                query: query.clone(),
                offset: 2,
                limit: 3,
            },
            Query::QueueBatch {
                query: query.clone(),
                after: None,
                limit: 3,
                expected: None,
            },
            Query::QueueEntries {
                query: query.clone(),
                queue_ids: vec![42, 43],
            },
        ] {
            encode_request_frame_into(
                &WireRequest::Query(WireQuery::from(expected.clone())),
                &mut encoded,
            )
            .unwrap();
            let WireRequest::Query(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
                panic!("expected query");
            };
            assert_eq!(Query::try_from(decoded).unwrap(), expected);
        }
    }
}

#[test]
fn playlist_group_requests_and_locations_round_trip_stable_identities() {
    let mut encoded = Vec::new();
    for query in [
        Query::PlaylistEntryLocations {
            query: None,
            playlist_id: 10,
            entry_ids: vec![42, i64::MAX, 42],
        },
        Query::DuplicateMemberSelection {
            fingerprint: blake3::hash(b"selected facts").to_hex().to_string(),
            track_ids: vec![42, i64::MAX, 42],
        },
    ] {
        let expected = serde_json::to_value(&query).unwrap();
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
    for command in [
        Command::InsertQueue {
            track_ids: vec![3, 1, 3],
            index: usize::MAX,
            expected_revision: u64::MAX,
        },
        Command::InsertQueueSources {
            directories: vec![PathBuf::from("/music")],
            track_ids: vec![3, 1],
            other_tracks: false,
            index: 0,
            expected_revision: 19,
        },
        Command::InsertPlaylist {
            playlist_id: 10,
            track_ids: vec![3, 1, 3],
            index: 7,
            expected_revision: 19,
        },
        Command::InsertPlaylistSources {
            playlist_id: 10,
            directories: vec![PathBuf::from("/music")],
            track_ids: vec![],
            other_tracks: false,
            index: 0,
            expected_revision: 19,
        },
        Command::MovePlaylistEntries {
            playlist_id: 10,
            entry_ids: vec![42],
            before_entry_id: Some(43),
            after: true,
            expected_revision: 19,
        },
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
            after: false,
        },
        Command::MovePlaylistEntries {
            playlist_id: 10,
            entry_ids: vec![],
            before_entry_id: None,
            expected_revision: 0,
            after: false,
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
        revisions: rivu_protocol::response::QueryRevisions {
            library: 5,
            bitrate: 15,
            structure: 6,
            album: 14,
            playlist: 7,
            queue: 8,
            playlist_entries: Some(rivu_protocol::response::PlaylistEntryRevision {
                playlist_id: 10,
                revision: 9,
            }),
            duplicate_groups: 10,
            duplicate_members: 11,
            track: Some(rivu_protocol::response::TrackRevision {
                track_id: 11,
                revision: 12,
            }),
            playlist_metadata: Some(rivu_protocol::response::PlaylistEntryRevision {
                playlist_id: 10,
                revision: 13,
            }),
        },
        result: Ok(
            rivu_protocol::response::ViewResponse::PlaylistEntryLocations(vec![
                rivu_protocol::model::PlaylistEntryLocation {
                    id: 42,
                    playlist_id: 10,
                    track_id: 11,
                    position: 1000,
                    offset: 1000,
                    total: 2000,
                },
            ]),
        ),
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
    use rivu_protocol::{
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
            library_committed: false,
            playlist_id: None,
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
fn library_locate_wire_round_trip_preserves_exact_identity_and_projection() {
    for tree in [false, true] {
        let query = Query::LibraryLocate {
            track_id: 42,
            path: "/音楽/Album".into(),
            tree,
            query: Some("needle".into()),
            favorite: Some(true),
            missing: Some(false),
            sort: rivu_protocol::model::LibrarySort::MostPlayed,
        };
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
fn library_root_query_wire_round_trip_preserves_paths_filters_and_pages() {
    use rivu_protocol::model::LibrarySort;
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
        vec!["/music".into(); rivu_protocol::model::MAX_QUERY_ROWS],
    ] {
        for query in [
            Query::LibraryRootCounts {
                roots: roots.clone(),
            },
            Query::LibraryRootStats { roots },
        ] {
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
}

#[test]
fn library_root_responses_wire_round_trip_preserves_projection_and_revisions() {
    use rivu_protocol::{
        model::{LibraryRootCount, LibraryRootStats, LibraryRow, Page},
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
        ViewResponse::OtherTracksCount(0),
        ViewResponse::OtherTracksCount(usize::MAX),
        ViewResponse::LibraryRootCounts(Vec::new()),
        ViewResponse::LibraryRootStats(vec![
            LibraryRootStats {
                path: "/音楽".into(),
                total: 3,
                file_size: u64::from(u32::MAX) + 1,
            },
            LibraryRootStats {
                path: "/empty".into(),
                total: 0,
                file_size: 0,
            },
        ]),
        ViewResponse::LibraryPage(Page {
            total: 4,
            rows: vec![LibraryRow {
                id: 42,
                path: "/音楽/track.wav".into(),
                title: "Été_100%".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration: Some(12.0),
                codec: "pcm".into(),
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: Some(16),
                cue_number: None,
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
                duplicate_groups: 9,
                duplicate_members: 10,
                ..Default::default()
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
    use crate::{config::Config, core::Runtime, store::Store};
    use rivu_protocol::{
        model::{LibrarySort, MAX_QUERY_ROWS},
        response::{QueryError, ViewResponse},
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
        library_roots: vec![root.clone(), empty.clone()],
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
    let remote = session.query(count_query.clone()).unwrap();
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
    let remote = session.query(page_query).unwrap();
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
        .request_ack(Command::SetFavorite {
            track_ids: vec![page.rows[0].id],
            favorite: true,
        })
        .unwrap();
    assert!(ack.is_success());
    let updated = session.query(count_query).unwrap();
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
            .query(Query::LibraryRootCounts {
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
fn playlist_content_lifetimes_and_batches_are_isolated_from_the_catalog() {
    use crate::{config::Config, core::Runtime, store::Store};
    use rivu_protocol::response::{PlaylistEntryRevision, QueryError, ViewResponse};
    let directory = socket_test_directory();
    let database = directory.path().join("library.db");
    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_title) VALUES (1,'one.wav','One'),(2,'two.wav','Two'),(3,'three.wav','Three');
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
        query: None,
        offset: 0,
        limit: 2,
    };
    let catalog = session.query(summaries.clone()).unwrap();
    let batch = |playlist_id, expected| Query::PlaylistEntryBatch {
        query: None,
        playlist_id,
        after: None,
        limit: 1,
        expected,
    };
    let first = session.query(batch(10, Some(catalog.revisions))).unwrap();
    let other = session.query(batch(20, Some(catalog.revisions))).unwrap();
    let ViewResponse::PlaylistEntryBatch(first_page) = first.result.as_ref().unwrap() else {
        panic!("expected first playlist batch")
    };
    let first_cursor = first_page.next.unwrap();
    let ViewResponse::PlaylistEntryBatch(other_page) = other.result.as_ref().unwrap() else {
        panic!("expected other playlist batch")
    };
    let other_cursor = other_page.next.unwrap();
    assert_eq!(first_page.rows[0].id, 101);
    assert_eq!(other_page.rows[0].id, 201);
    let sections = StateSections {
        library: true,
        ..StateSections::default()
    };
    let before = session.get_state(sections).unwrap();
    let initial = before.library.as_ref().unwrap();
    assert_eq!(initial.playlist_entry_revisions.len(), 2);
    assert_eq!(
        initial.playlist_entry_revisions.get(&10),
        Some(&first.revisions.playlist_entries.unwrap().revision)
    );
    assert_eq!(
        initial.playlist_entry_revisions.get(&20),
        Some(&other.revisions.playlist_entries.unwrap().revision)
    );
    assert_ne!(
        first.revisions.playlist_entries.unwrap().revision,
        other.revisions.playlist_entries.unwrap().revision
    );
    // A catalog baseline can start a traversal, never continue one without E.
    assert_eq!(
        session
            .query(Query::PlaylistEntryBatch {
                query: None,
                playlist_id: 10,
                after: Some(first_cursor),
                limit: 1,
                expected: Some(catalog.revisions),
            })
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                before_entry_id: None,
                after: false,
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let after = session.get_state(sections).unwrap();
    assert_ne!(before.revisions.library, after.revisions.library);
    let library = after.library.unwrap();
    assert_eq!(library.playlist_revision, catalog.revisions.playlist);
    assert_eq!(library.revision, catalog.revisions.library);
    let moved_token = *library.playlist_entry_revisions.get(&10).unwrap();
    assert_ne!(
        moved_token,
        first.revisions.playlist_entries.unwrap().revision
    );
    assert_eq!(library.playlist_entry_revisions.len(), 2);
    assert_eq!(
        library.playlist_entry_revisions.get(&20),
        Some(&other.revisions.playlist_entries.unwrap().revision)
    );
    assert_eq!(
        library.duplicate_members_revision,
        initial.duplicate_members_revision
    );
    assert_eq!(
        serde_json::to_value(session.query(summaries).unwrap()).unwrap(),
        serde_json::to_value(&catalog).unwrap()
    );
    assert_eq!(
        session
            .query(batch(10, Some(first.revisions)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .query(batch(20, Some(other.revisions)))
            .unwrap()
            .result
            .is_ok()
    );
    let selected = session
        .query(Query::PlaylistEntry { entry_id: 101 })
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
    let location_query = Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 10,
        entry_ids: vec![101, 201, i64::MAX],
    };
    let locations = session.query(location_query.clone()).unwrap();
    assert_eq!(
        location_query.stamp(locations.revisions),
        location_query.stamp(selected.revisions)
    );
    assert!(
        matches!(locations.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].id == 101 && rows[0].position == 1)
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: moved_token,
            })
            .unwrap()
            .is_success()
    );
    let removed = session
        .query(Query::PlaylistEntryLocations {
            query: None,
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
    let expected = removed.revisions;
    assert!(
        session
            .request_ack(Command::RenamePlaylist {
                playlist_id: 10,
                name: "renamed".into()
            })
            .unwrap()
            .is_success()
    );
    let library = session.get_state(sections).unwrap().library.unwrap();
    assert_ne!(library.playlist_revision, expected.playlist);
    assert_eq!(
        library.duplicate_members_revision,
        expected.duplicate_members
    );
    assert_eq!(library.playlist_entry_revisions.len(), 2);
    assert_eq!(
        library.playlist_entry_revisions.get(&10),
        Some(&expected.playlist_entries.unwrap().revision)
    );
    assert!(
        session
            .query(batch(10, Some(expected)))
            .unwrap()
            .result
            .is_ok()
    );
    // Renames and membership changes elsewhere preserve a real continuation.
    assert!(
        session
            .request_ack(Command::RenamePlaylist {
                playlist_id: 20,
                name: "other renamed".into(),
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: 10,
                track_ids: vec![3],
            })
            .unwrap()
            .is_success()
    );
    let member_change = session.query(batch(10, None)).unwrap();
    assert_ne!(
        member_change.revisions.playlist_entries,
        expected.playlist_entries
    );
    assert_ne!(member_change.revisions.playlist, expected.playlist);
    assert_eq!(member_change.revisions.library, expected.library);
    assert!(member_change.result.is_ok());
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![],
                expected_revision: expected.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let continuation = session
        .query(Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 20,
            after: Some(other_cursor),
            limit: 1,
            expected: Some(other.revisions),
        })
        .unwrap();
    assert!(matches!(continuation.result,
        Ok(ViewResponse::PlaylistEntryBatch(page)) if page.rows.len() == 1
            && page.rows[0].id == 202 && page.next.is_none()));
    assert_eq!(
        continuation.revisions.playlist_entries,
        other.revisions.playlist_entries
    );
    assert_ne!(continuation.revisions.playlist, other.revisions.playlist);
    assert_ne!(
        continuation.revisions.duplicate_members,
        other.revisions.duplicate_members
    );
    assert_eq!(
        continuation.revisions.duplicate_groups,
        other.revisions.duplicate_groups
    );
    // P is irrelevant even when an unscoped baseline starts after catalog changes.
    assert!(
        session
            .query(batch(20, Some(catalog.revisions)))
            .unwrap()
            .result
            .is_ok()
    );
    let mut stale_library = catalog.revisions;
    stale_library.library = stale_library.library.wrapping_add(1);
    assert_eq!(
        session
            .query(batch(20, Some(stale_library)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    // The current scalar token stays usable across every catalog-only change.
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                before_entry_id: None,
                after: false,
                expected_revision: other.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let current = session.query(batch(20, None)).unwrap();
    assert_ne!(
        current.revisions.playlist_entries,
        other.revisions.playlist_entries
    );
    assert_eq!(
        session
            .query(Query::PlaylistEntryBatch {
                query: None,
                playlist_id: 20,
                after: Some(other_cursor),
                limit: 1,
                expected: Some(other.revisions),
            })
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                expected_revision: other.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let before_noop = session.get_state(sections).unwrap().library.unwrap();
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: 20,
                track_ids: vec![],
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                before_entry_id: None,
                after: false,
                expected_revision: current.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let after_noop = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(
        before_noop.playlist_entry_revisions,
        after_noop.playlist_entry_revisions
    );
    assert_eq!(before_noop.playlist_revision, after_noop.playlist_revision);
    assert_eq!(
        before_noop.duplicate_members_revision,
        after_noop.duplicate_members_revision
    );
    // Deletion removes the lifetime, rather than falling back to the catalog token.
    assert!(
        session
            .request_ack(Command::DeletePlaylist { playlist_id: 20 })
            .unwrap()
            .is_success()
    );
    let deleted = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(deleted.playlist_entry_revisions.len(), 1);
    assert!(!deleted.playlist_entry_revisions.contains_key(&20));
    assert_eq!(
        deleted.playlist_entry_revisions.get(&10),
        before_noop.playlist_entry_revisions.get(&10)
    );
    assert_eq!(
        session
            .query(batch(20, Some(current.revisions)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let missing = session.query(batch(20, None)).unwrap();
    assert!(missing.result.is_err());
    assert!(missing.revisions.playlist_entries.is_none());
    for playlist_id in [20, i64::MAX] {
        assert!(
            !session
                .request_ack(Command::RemovePlaylistEntries {
                    playlist_id,
                    entry_ids: vec![],
                    expected_revision: current.revisions.playlist_entries.unwrap().revision,
                })
                .unwrap()
                .is_success()
        );
        assert!(
            !session
                .request_ack(Command::MovePlaylistEntries {
                    playlist_id,
                    entry_ids: vec![],
                    before_entry_id: None,
                    after: false,
                    expected_revision: current.revisions.playlist_entries.unwrap().revision,
                })
                .unwrap()
                .is_success()
        );
    }
    assert!(
        session
            .request_ack(Command::CreatePlaylist {
                name: "replacement".into()
            })
            .unwrap()
            .is_success()
    );
    let recreated = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(recreated.playlist_entry_revisions.len(), 2);
    assert_eq!(
        recreated.playlist_entry_revisions.get(&10),
        deleted.playlist_entry_revisions.get(&10)
    );
    let (&new_id, &new_revision) = recreated
        .playlist_entry_revisions
        .iter()
        .find(|(id, _)| **id != 10)
        .unwrap();
    assert_ne!(
        new_revision,
        current.revisions.playlist_entries.unwrap().revision
    );
    assert_ne!(
        new_revision,
        first.revisions.playlist_entries.unwrap().revision
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: new_id,
                entry_ids: vec![],
                expected_revision: current.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: new_id,
                entry_ids: vec![],
                expected_revision: new_revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: new_id,
                track_ids: vec![1],
            })
            .unwrap()
            .is_success()
    );
    let populated = session.query(batch(new_id, None)).unwrap();
    let ViewResponse::PlaylistEntryBatch(page) = populated.result.unwrap() else {
        panic!("expected populated playlist batch")
    };
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntry {
                entry_id: page.rows[0].id,
            })
            .unwrap()
            .is_success()
    );
    let emptied = session.query(batch(new_id, None)).unwrap();
    assert_ne!(
        emptied.revisions.playlist_entries,
        populated.revisions.playlist_entries
    );
    assert!(
        matches!(emptied.result, Ok(ViewResponse::PlaylistEntryBatch(page)) if page.rows.is_empty())
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
        status: rivu_protocol::response::AckStatus::Failed,
        error: Some("failed".into()),
        revision: 42,
        scan_id: Some(u64::MAX),
        relocation_id: Some(42),
        processing_id: Some(7),
        database_optimization: None,
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
    assert_eq!(decoded.database_optimization, ack.database_optimization);
}

#[test]
fn query_wire_response_has_no_state_queue_payload() {
    let response = QueryResponse {
        revisions: rivu_protocol::response::QueryRevisions::default(),
        result: Err(rivu_protocol::response::QueryError::Message(
            "query failed".into(),
        )),
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
        library: Some(rivu_protocol::model::LibrarySnapshot::default()),
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
fn system_scan_task_identity_wire_round_trip_preserves_boundaries() {
    for scan_task_id in [None, Some(0), Some(42), Some(u64::MAX)] {
        let system = rivu_protocol::model::SystemState {
            scan_task_id,
            scanning: scan_task_id.is_some(),
            ..Default::default()
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
        let WireResponse::State(state) = decode_response_frame(&encoded[4..]).unwrap() else {
            panic!("expected state")
        };
        let system = state_response(state).system.unwrap();
        assert_eq!(system.scan_task_id, scan_task_id);
        assert_eq!(system.scanning, scan_task_id.is_some());
    }
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
            scan_min_duration_secs: if output_device.is_some() { u32::MAX } else { 0 },
            spectrogram_flow_speed: if output_device.is_some() { 2.0 } else { 0.1 },
            spectrogram_accumulation: if output_device.is_some() { 95 } else { 10 },
            output_device,
            library_columns,
            ui_theme: crate::theme::Theme::TokyoNight,
            album_artwork_style: crate::config::AlbumArtworkStyle::Metal,
            album_artwork_mode: crate::config::AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 12.5,
            album_artwork_direction: crate::config::AlbumArtworkDirection::Counterclockwise,
            album_artwork_fps: 240,
            visualization_fps: 240,
            ..Default::default()
        });
        let system = rivu_protocol::model::SystemState {
            config: config.clone(),
            revision: 42,
            ..rivu_protocol::model::SystemState::default()
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
            sort: rivu_protocol::model::LibrarySort::Album,
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
        WireRequest::WaitTask(rivu_protocol::model::TaskTarget::Scan { id: 42 }),
    ];
    for request in requests {
        let expected_type = match &request {
            WireRequest::Hello(_) => RequestType::Hello,
            WireRequest::State(_) => RequestType::State,
            WireRequest::Query(_) => RequestType::Query,
            WireRequest::Command(_) => RequestType::Command,
            WireRequest::Watch(_) => RequestType::Watch,
            WireRequest::WaitTask(_) => RequestType::WaitTask,
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
            WireRequest::WaitTask(target) => {
                assert_eq!(target, rivu_protocol::model::TaskTarget::Scan { id: 42 })
            }
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
            scan_min_duration_secs: if output_device.is_some() { u32::MAX } else { 0 },
            spectrogram_flow_speed: if output_device.is_some() { 2.0 } else { 0.1 },
            spectrogram_accumulation: if output_device.is_some() { 95 } else { 10 },
            output_device,
            library_columns,
            ui_theme: crate::theme::Theme::TokyoNightDay,
            album_artwork_style: crate::config::AlbumArtworkStyle::Duotone,
            album_artwork_mode: crate::config::AlbumArtworkMode::Rotating,
            album_artwork_period_seconds: 12.5,
            album_artwork_direction: crate::config::AlbumArtworkDirection::Counterclockwise,
            album_artwork_fps: 30,
            visualization_fps: 30,
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
fn relative_playback_intents_round_trip() {
    let mut encoded = Vec::new();
    for command in [
        Command::AdjustVolume { delta: -0.05 },
        Command::AdjustVolume { delta: 0.05 },
        Command::CycleRepeat,
        Command::ToggleShuffle,
        Command::SeekRelative {
            queue_id: u64::MAX,
            delta: -5.0,
        },
        Command::SeekRelative {
            queue_id: 7,
            delta: 5.0,
        },
        Command::SeekQueue {
            queue_id: 7,
            seconds: 0.0,
        },
        Command::Seek { seconds: 42.0 },
    ] {
        let expected = serde_json::to_value(&command).unwrap();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(command)),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected playback intent");
        };
        assert_eq!(
            serde_json::to_value(Command::try_from(wire).unwrap()).unwrap(),
            expected
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

#[test]
fn incompatible_versions_are_rejected_before_any_body_decode_including_hello() {
    async_io::block_on(async {
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
    });
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
        revisions: rivu_protocol::response::QueryRevisions::default(),
        result: Ok(rivu_protocol::response::ViewResponse::PlaylistSummaries(
            rivu_protocol::model::Page {
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

#[test]
fn oversized_frame_is_rejected_before_consuming_or_reserving_its_body() {
    async_io::block_on(async {
        for limit in [MAX_REQUEST, MAX_RESPONSE] {
            for length in [(limit + 1) as u32, u32::MAX] {
                let mut encoded = length.to_le_bytes().to_vec();
                encoded.push(0xaa);
                let mut stream = encoded.as_slice();
                let mut pending = bytes::BytesMut::new();
                if limit == MAX_REQUEST {
                    assert!(read_request_frame(&mut stream, &mut pending).await.is_err());
                } else {
                    assert!(
                        read_response_frame(&mut stream, &mut pending)
                            .await
                            .is_err()
                    );
                }
                assert_eq!(stream, &[0xaa]);
                assert_eq!(pending.len(), 4);
                assert!(pending.capacity() < limit);
            }
        }
    });
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

#[test]
fn response_frame_boundary_preserves_the_next_message() {
    async_io::block_on(async {
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
    });
}

#[test]
fn segmented_frame_reads_preserve_payload_and_leave_the_next_frame_unread() {
    async_io::block_on(async {
        use futures::io::AsyncReadExt;

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
            assert!(matches!(
                read_response_frame(&mut stream, &mut pending).await.unwrap(),
                WireResponse::Error(message) if message == "segmented response".repeat(1024)
            ));
            assert!(pending.is_empty());
            let mut unread = Vec::new();
            stream.read_to_end(&mut unread).await.unwrap();
            assert_eq!(unread, second);
        }
    });
}

#[test]
fn cancelled_frame_reads_resume_from_every_partial_prefix_and_body() {
    async_io::block_on(async {
        use futures::io::AsyncWriteExt;
        use std::{
            future::Future,
            task::{Context, Waker},
        };

        let mut encoded = Vec::new();
        let mut second = Vec::new();
        encode_response_frame_into(&WireResponse::Error("resumed".into()), &mut encoded).unwrap();
        encode_response_frame_into(&WireResponse::Error("next".into()), &mut second).unwrap();

        for split in 1..encoded.len() {
            let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut writer = async_io::Async::new(writer).unwrap();
            let mut reader = async_io::Async::new(reader).unwrap();
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
            writer
                .get_ref()
                .shutdown(std::net::Shutdown::Write)
                .unwrap();
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
    });
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
        revisions: rivu_protocol::response::QueryRevisions::default(),
        result: Ok(rivu_protocol::response::ViewResponse::PlaylistSummaries(
            rivu_protocol::model::Page {
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
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
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
                status: rivu_protocol::response::AckStatus::Failed,
                error: Some("rejected".into()),
                revision: 3,
                scan_id: None,
                relocation_id: None,
                processing_id: None,
                database_optimization: None,
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
        query: None,
        offset: 0,
        limit: 20,
    };
    let response = session.query(query).unwrap();
    assert!(
        matches!(response.result, Ok(rivu_protocol::response::ViewResponse::PlaylistSummaries(page)) if page.total == 40)
    );
    let ack = session
        .request_ack(Command::CreatePlaylist { name: "new".into() })
        .unwrap();
    assert!(!ack.is_success());
    assert_eq!(ack.revision, 3);
    let query = Query::PlaylistSummaries {
        query: None,
        offset: 20,
        limit: 20,
    };
    assert!(session.query(query.clone()).is_err());
    let response = session.query(query).unwrap();
    assert!(
        matches!(response.result, Ok(rivu_protocol::response::ViewResponse::PlaylistSummaries(page)) if page.total == 7)
    );
    server.join().unwrap();
}

#[test]
fn client_session_discards_partial_failed_response_before_next_explicit_request() {
    use std::{io::Write, os::unix::net::UnixListener};

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
        );
        let mut frame = Vec::new();
        encode_response_frame_into(
            &WireResponse::Ack(Ack {
                status: rivu_protocol::response::AckStatus::Succeeded,
                error: None,
                revision: 1,
                scan_id: None,
                relocation_id: None,
                processing_id: None,
                database_optimization: None,
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
                status: rivu_protocol::response::AckStatus::Succeeded,
                error: None,
                revision: 2,
                scan_id: None,
                relocation_id: None,
                processing_id: None,
                database_optimization: None,
            },
        );
    });
    let mut session = client_session(directory.path()).unwrap();
    session.bind_instance(1).unwrap();
    let error = session
        .request_ack(Command::CreatePlaylist {
            name: "first".into(),
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<CommandFailure>().unwrap().status,
        rivu_protocol::response::AckStatus::Unknown
    );
    let ack = session
        .request_ack(Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap();
    assert!(ack.is_success());
    assert_eq!(ack.revision, 2);
    server.join().unwrap();
}

#[test]
fn cancelling_client_session_closes_pending_watch_with_partial_response() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let stop = session.cancellation();
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
    stop.cancel();
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
    let address = rivu_protocol::protocol::address(directory.path()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind_addr(&address).unwrap();
    let client = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
    let accepted = listener.accept().unwrap().0;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    for stream in [client, accepted] {
        use std::os::fd::AsRawFd;
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `length` bytes to the valid credential buffer.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut length,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(length as usize, std::mem::size_of::<libc::ucred>());
        // SAFETY: successful SO_PEERCRED returned the complete credential structure.
        let credentials = unsafe { credentials.assume_init() };
        assert_eq!(credentials.uid, uid);
        assert_eq!(credentials.gid, gid);
        assert!(rivu_protocol::protocol::verify_peer(&stream).is_ok());
    }
}

#[test]
fn pinned_session_rejects_initial_replacement_before_commands_queries_or_state() {
    use std::{io::Read, os::unix::net::UnixListener};
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
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
        .request_ack(Command::CreatePlaylist {
            name: "A-only".into(),
        })
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    assert_eq!(
        error.downcast_ref::<CommandFailure>().unwrap().status,
        rivu_protocol::response::AckStatus::NotAdmitted
    );
    let error = session
        .query(Query::PlaylistSummaries {
            query: None,
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
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
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
            .request_ack(Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    let error = session
        .request_ack(Command::CreatePlaylist {
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
        UnixListener::bind_addr(&rivu_protocol::protocol::address(directory.path()).unwrap())
            .unwrap();
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
                status: rivu_protocol::response::AckStatus::Succeeded,
                error: None,
                revision: 2,
                scan_id: None,
                relocation_id: None,
                processing_id: None,
                database_optimization: None,
            },
        );
    });
    let mut session = client_session(directory.path()).unwrap();
    assert!(
        session
            .request_ack(Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    assert_eq!(session.instance_id(), None);
    assert!(
        session
            .request_ack(Command::CreatePlaylist {
                name: "second".into()
            })
            .unwrap()
            .is_success()
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
    let address = rivu_protocol::protocol::address(&data_dir).unwrap();
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
        let address = rivu_protocol::protocol::address(alias).unwrap();
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
        let address = rivu_protocol::protocol::address(data_dir).unwrap();
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
        let address = rivu_protocol::protocol::address(directory.path()).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    });
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn album_queries_and_commands_round_trip_exact_keys() {
    let key = rivu_protocol::model::AlbumKey {
        directory: "/音楽/album".into(),
        title: String::new(),
    };
    for query in [
        Query::Albums {
            query: Some("artist 音楽".into()),
            sort: rivu_protocol::model::AlbumSort::Artist,
            offset: 24,
            limit: 24,
        },
        Query::Albums {
            query: None,
            sort: rivu_protocol::model::AlbumSort::Title,
            offset: 0,
            limit: 24,
        },
        Query::AlbumTracks {
            query: None,
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
fn collection_commands_round_trip_source_and_destination_versions() {
    use rivu_protocol::model::{
        AlbumKey, CollectionDestination, CollectionInsertion, CollectionSource,
    };
    for source in [
        CollectionSource::Album {
            key: AlbumKey {
                directory: "/music/专辑".into(),
                title: "Exact title ".into(),
            },
            revision: u64::MAX,
        },
        CollectionSource::Playlist {
            playlist_id: i64::MAX,
            revision: u64::MAX,
        },
    ] {
        for destination in [
            CollectionDestination::Queue { insertion: None },
            CollectionDestination::Queue {
                insertion: Some(CollectionInsertion {
                    index: u64::MAX,
                    revision: 8,
                }),
            },
            CollectionDestination::Playlist {
                playlist_id: 4,
                revision: 17,
                index: None,
            },
            CollectionDestination::Playlist {
                playlist_id: 4,
                revision: 19,
                index: Some(u64::MAX),
            },
        ] {
            let command = Command::TransferCollection {
                source: source.clone(),
                destination,
            };
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
}

#[test]
fn queue_queries_and_full_queue_playlist_commands_round_trip() {
    use rivu_protocol::model::QueueCursor;
    for query in [
        Query::QueuePage {
            query: None,
            offset: 199_936,
            limit: 256,
        },
        Query::QueueBatch {
            query: None,
            after: Some(rivu_protocol::model::BatchContinuation {
                cursor: QueueCursor {
                    position: 255,
                    id: 1000,
                },
                offset: 128,
                scope: [42; 32],
            }),
            limit: 256,
            expected: Some(rivu_protocol::response::QueryRevisions {
                queue: 42,
                ..Default::default()
            }),
        },
        Query::PlaylistEntryBatch {
            query: Some("needle".into()),
            playlist_id: 10,
            after: Some(rivu_protocol::model::BatchContinuation {
                cursor: rivu_protocol::model::PlaylistEntryCursor {
                    position: 1023,
                    id: 5000,
                },
                offset: 256,
                scope: [17; 32],
            }),
            limit: 64,
            expected: None,
        },
        Query::QueueEntries {
            query: None,
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
fn filtered_occurrence_continuations_round_trip_and_reject_stale_ipc_scopes() {
    use rivu_protocol::response::{QueryError, ViewResponse};
    let directory = socket_test_directory();
    let database = directory.path().join("library.db");
    drop(crate::store::Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.execute_batch(
        "INSERT INTO tracks(id,path,raw_title) VALUES
             (1,'one.wav','other'),(2,'two.wav','needle'),(3,'three.wav','other'),
             (4,'four.wav','other'),(5,'five.wav','needle'),(6,'six.wav','other'),(7,'seven.wav','needle');
         INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES (0,1,0),(0,2,1),(0,2,2),(0,3,3),(0,4,4),(0,5,5),(0,6,6),(0,7,7);
         INSERT INTO playlists(id,name) VALUES(10,'first'),(20,'second');
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 10,id,id-1 FROM tracks;
         INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(10,2,7);
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 20,id,id-1 FROM tracks;",
    ).unwrap();
    drop(connection);
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
    let server = Server::start(&owner, runtime.handle.clone(), app.clone()).unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let queue_query = |query: &str, after| Query::QueueBatch {
        query: Some(query.into()),
        after,
        limit: 1,
        expected: None,
    };
    let entry_query = |playlist_id, query: &str, after| Query::PlaylistEntryBatch {
        playlist_id,
        query: Some(query.into()),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first_queue = None;
    let mut positions = Vec::new();
    loop {
        let ViewResponse::QueueBatch(batch) = session
            .query(queue_query("needle", after))
            .unwrap()
            .result
            .unwrap()
        else {
            panic!("expected queue batch")
        };
        assert_eq!(batch.rows[0].offset, positions.len());
        positions.push(batch.rows[0].position);
        after = batch.next;
        if first_queue.is_none() {
            first_queue = after;
        }
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, positions.len());
    }
    assert_eq!(positions, [1, 2, 5, 7]);
    assert_eq!(
        session
            .query(queue_query("other", first_queue))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let mut after = None;
    let mut first_entry = None;
    let mut positions = Vec::new();
    loop {
        let ViewResponse::PlaylistEntryBatch(batch) = session
            .query(entry_query(10, "needle", after))
            .unwrap()
            .result
            .unwrap()
        else {
            panic!("expected playlist batch")
        };
        assert_eq!(batch.rows[0].offset, positions.len());
        positions.push(batch.rows[0].position);
        after = batch.next;
        if first_entry.is_none() {
            first_entry = after;
        }
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, positions.len());
    }
    assert_eq!(positions, [1, 4, 6, 7]);
    assert_eq!(
        session
            .query(entry_query(20, "needle", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(
        session
            .query(entry_query(10, "other", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .request_ack(Command::EditTrack {
                track_id: 2,
                title: Some("other".into()),
                artist: None,
                album: None,
                decoder_hint: None,
            })
            .unwrap()
            .is_success()
    );
    assert_eq!(
        session
            .query(queue_query("needle", first_queue))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(
        session
            .query(entry_query(10, "needle", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(restarted) = session
        .query(entry_query(10, "needle", None))
        .unwrap()
        .result
        .unwrap()
    else {
        panic!("expected restarted playlist batch")
    };
    assert_eq!(
        (restarted.rows[0].position, restarted.rows[0].offset),
        (4, 0)
    );
    drop(session);
    drop(server);
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
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

#[test]
fn processing_commands_and_configuration_use_current_positional_contract() {
    use rivu_protocol::model::{
        EqBandEdit, EqualizerEdit, NeuralPatch, NoiseSelection, ProcessingOperation,
    };
    let commands = [
        Command::StartProcessing {
            track_id: 17,
            operation: ProcessingOperation::Denoise,
            profile: Some(NoiseSelection {
                start_seconds: 1.0,
                end_seconds: 2.0,
            }),
        },
        Command::EqualizerEdit {
            edit: EqualizerEdit::BandField {
                index: 2,
                field: EqBandEdit::Position {
                    frequency_hz: 1200.0,
                    gain_db: 4.0,
                },
            },
        },
        Command::NeuralEdit {
            patch: Box::new(NeuralPatch {
                speech_manifest: Some(None),
                threads: Some(2),
                ..Default::default()
            }),
        },
    ];
    for command in commands {
        let expected = serde_json::to_value(&command).unwrap();
        let wire = WireCommand::from(command);
        let bytes = postcard::to_allocvec(&wire).unwrap();
        let decoded: WireCommand = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(Command::try_from(decoded).unwrap()).unwrap(),
            expected
        );
    }
    let mut config = crate::config::Config::default();
    config.equalizer.enabled = true;
    config
        .equalizer
        .bands
        .push(rivu_dsp::equalizer::config::EqBand::default());
    config.neural.speech_manifest = Some("/models/speech.json".into());
    config.neural.separation_overlap = 0.5;
    config.spectrogram_flow_speed = 1.25;
    config.spectrogram_accumulation = 85;
    let wire = WireConfig::from(&config);
    let bytes = postcard::to_allocvec(&wire).unwrap();
    let decoded: WireConfig = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(crate::config::Config::from(decoded), config);
}

#[test]
fn graph_command_frames_preserve_revision_typed_edit_targets_modes_and_export_paths() {
    use rivu_protocol::model::{GraphEdit, GraphExecutionMode, GraphNodeKind, GraphPort};
    let port = GraphPort {
        node: u64::MAX,
        port: u16::MAX,
    };
    let mut commands = vec![
        Command::ProcessingGraphEdit {
            expected_revision: u64::MAX,
            edit: Box::new(GraphEdit::Replace {
                id: 7,
                kind: GraphNodeKind::Mix {
                    gains_db: vec![-6.0, -12.5],
                    muted: vec![false, true],
                    solo: vec![true, false],
                },
            }),
        },
        Command::ProcessingGraphAudition {
            id: u64::MAX,
            node: port,
        },
        Command::ProcessingGraphExport {
            id: 91,
            node: port,
            path: "/rendered outputs/音楽 -6 dB.wav".into(),
        },
    ];
    commands.extend(
        [
            GraphExecutionMode::Auto,
            GraphExecutionMode::Offline,
            GraphExecutionMode::Realtime,
            GraphExecutionMode::Hybrid,
        ]
        .map(|mode| Command::ProcessingGraphRun {
            track_id: 42,
            target: port,
            mode,
        }),
    );
    for command in commands {
        let expected = serde_json::to_value(&command).unwrap();
        let mut encoded = Vec::new();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(command)),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected graph command frame");
        };
        assert_eq!(
            serde_json::to_value(Command::try_from(decoded).unwrap()).unwrap(),
            expected,
        );
    }
}

#[test]
fn graph_configuration_round_trip_retains_authoritative_revision_nodes_and_edges() {
    use rivu_protocol::model::{GraphEdit, GraphNodeKind};
    let mut config = crate::config::Config::default();
    config
        .processing_graph
        .edit(
            0,
            GraphEdit::Add {
                kind: GraphNodeKind::Mix {
                    gains_db: vec![-6.0, -12.5],
                    muted: vec![false, true],
                    solo: vec![false, false],
                },
                position: [-40.0, 75.0],
            },
        )
        .unwrap();
    let mut encoded = Vec::new();
    encode_request_frame_into(
        &WireRequest::Command(WireCommand::from(Command::Configure {
            config: Arc::new(config.clone()),
        })),
        &mut encoded,
    )
    .unwrap();
    let WireRequest::Command(decoded) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("expected configure frame");
    };
    let Command::Configure { config: decoded } = Command::try_from(decoded).unwrap() else {
        panic!("expected configure command");
    };
    assert_eq!(decoded.as_ref(), &config);
}

#[test]
fn graph_task_query_frame_retains_pcm_availability_and_context_without_paths() {
    use rivu_protocol::model::{
        GraphExecutionMode, GraphNodeOutput, GraphPort, ProcessingGraph, ProcessingOperation,
        ProcessingPhase, ProcessingSettings, ProcessingTaskStatus,
    };
    let target = GraphPort { node: 2, port: 0 };
    let task = ProcessingTaskStatus {
        id: 91,
        track_id: 42,
        operation: ProcessingOperation::Graph,
        phase: ProcessingPhase::Complete,
        completed: 48_000,
        total: Some(48_000),
        error: None,
        outputs: Some(rivu_protocol::model::ProcessingOutputs {
            denoised: true,
            vocals: false,
            accompaniment: true,
        }),
        sample_rate: Some(48_000),
        channels: Some(2),
        frames: Some(48_000),
        model_id: None,
        model_sha256: None,
        source_path: "/source music/original.wav".into(),
        source_version: "original-version".into(),
        source_cue: None,
        settings: ProcessingSettings {
            denoise: Default::default(),
            neural: Default::default(),
            profile: None,
            graph: Some(ProcessingGraph::default()),
            graph_target: Some(target),
            graph_mode: Some(GraphExecutionMode::Offline),
            graph_boundary: Some(GraphPort { node: 1, port: 0 }),
            graph_export_path: Some("/rendered outputs/voice -6 dB.wav".into()),
        },
        graph_revision: Some(0),
        graph_run_id: Some(90),
        graph_outputs: vec![
            GraphNodeOutput {
                port: GraphPort { node: 1, port: 0 },
                context: None,
            },
            GraphNodeOutput {
                port: target,
                context: Some(rivu_protocol::model::GraphOutputContext { start_frame: -256 }),
            },
        ],
        graph_exported_path: Some("/rendered outputs/voice -6 dB.wav".into()),
        graph_nodes: Vec::new(),
        graph_plan: None,
    };
    let json = serde_json::to_value(&task).unwrap();
    assert_eq!(
        json["outputs"],
        serde_json::json!({"denoised": true, "vocals": false, "accompaniment": true})
    );
    assert!(json["graph_outputs"][0].get("path").is_none());
    assert!(json["graph_outputs"][1]["context"].get("path").is_none());
    let mut encoded = Vec::new();
    encode_response_frame_into(
        &WireResponse::Query(QueryResponse {
            revisions: Default::default(),
            result: Ok(rivu_protocol::response::ViewResponse::ProcessingTask(Some(
                Box::new(task.clone()),
            ))),
        }),
        &mut encoded,
    )
    .unwrap();
    let WireResponse::Query(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected graph task response");
    };
    let rivu_protocol::response::ViewResponse::ProcessingTask(Some(decoded)) =
        decoded.result.unwrap()
    else {
        panic!("expected retained graph task");
    };
    assert_eq!(*decoded, task);
}
