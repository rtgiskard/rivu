use crate as rivu_protocol;
use crate::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
    transport::*,
    wire::*,
};
use std::{path::PathBuf, sync::Arc};

#[test]
fn master_bypass_wire_requires_explicit_boolean_and_preserves_direction() {
    for bypassed in [false, true] {
        let mut encoded = Vec::new();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(Command::ProcessingBypass { bypassed })),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected command");
        };
        assert!(matches!(Command::try_from(command).unwrap(),
            Command::ProcessingBypass { bypassed: actual } if actual == bypassed));
        let json = serde_json::json!({
            "command": "processing_bypass",
            "bypassed": bypassed,
        });
        assert!(matches!(serde_json::from_value::<Command>(json).unwrap(),
            Command::ProcessingBypass { bypassed: actual } if actual == bypassed));
    }
    assert!(serde_json::from_str::<Command>(r#"{"command":"processing_bypass"}"#).is_err());
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!("true"),
        serde_json::json!(1),
    ] {
        assert!(
            serde_json::from_value::<Command>(serde_json::json!({
                "command": "processing_bypass",
                "bypassed": invalid,
            }))
            .is_err()
        );
    }
}

#[test]
fn processing_toggle_intents_preserve_typed_targets_on_wire_and_json() {
    use crate::model::ProcessingToggle;
    let mut encoded = Vec::new();
    for (target, name, position) in [
        (ProcessingToggle::Bypass, "bypass", 0),
        (ProcessingToggle::Hum, "hum", 1),
        (ProcessingToggle::Impulse, "impulse", 2),
        (ProcessingToggle::SpeechPostFilter, "speech_post_filter", 3),
    ] {
        assert_eq!(postcard::to_allocvec(&target).unwrap(), [position]);
        let command = Command::ToggleProcessing { target };
        let json = serde_json::json!({"command": "toggle_processing", "target": name});
        assert_eq!(serde_json::to_value(&command).unwrap(), json);
        assert!(matches!(serde_json::from_value::<Command>(json).unwrap(),
            Command::ToggleProcessing { target: actual } if actual == target));
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(command)),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected processing toggle");
        };
        assert!(matches!(Command::try_from(wire).unwrap(),
            Command::ToggleProcessing { target: actual } if actual == target));
    }
    for json in [
        r#"{"command":"toggle_processing"}"#,
        r#"{"command":"toggle_processing","target":"equalizer"}"#,
        r#"{"command":"toggle_processing","target":true}"#,
    ] {
        assert!(serde_json::from_str::<Command>(json).is_err());
    }
}

#[test]
fn exact_playlist_summary_round_trips_ids_and_optional_results() {
    use crate::{model::PlaylistSummary, response::ViewResponse};
    let mut encoded = Vec::new();
    for playlist_id in [i64::MIN, 0, 42, i64::MAX] {
        let query = Query::PlaylistSummary { playlist_id };
        encode_request_frame_into(
            &WireRequest::Query(WireQuery::from(query.clone())),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected playlist summary query");
        };
        assert_eq!(Query::try_from(wire).unwrap(), query);
        for summary in [
            None,
            Some(PlaylistSummary {
                id: playlist_id,
                name: "Été playlist".into(),
                entry_count: 3,
            }),
        ] {
            let response = QueryResponse {
                revisions: Default::default(),
                result: Ok(ViewResponse::PlaylistSummary(summary.clone())),
            };
            encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
            let WireResponse::Query(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
                panic!("expected playlist summary response");
            };
            let ViewResponse::PlaylistSummary(decoded) = decoded.result.unwrap() else {
                panic!("expected optional playlist summary");
            };
            assert_eq!(decoded, summary);
        }
    }
}

#[test]
fn exact_playlist_summary_identity_and_stamp_select_catalog() {
    use crate::response::QueryRevisions;
    let query = Query::PlaylistSummary { playlist_id: 42 };
    assert_ne!(
        query.identity(),
        Query::PlaylistSummary { playlist_id: 43 }.identity()
    );
    let revisions = QueryRevisions {
        playlist: 7,
        library: 8,
        bitrate: 9,
        queue: 10,
        ..Default::default()
    };
    assert_eq!(
        query.stamp(revisions).revisions(),
        QueryRevisions {
            playlist: 7,
            ..Default::default()
        }
    );
    assert_ne!(
        query.stamp(revisions),
        query.stamp(QueryRevisions {
            playlist: 8,
            ..revisions
        })
    );
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
    let mut assert_query_round_trip = |expected: Query| {
        encode_request_frame_into(
            &WireRequest::Query(WireQuery::from(expected.clone())),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Query(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected query")
        };
        assert_eq!(Query::try_from(wire).unwrap(), expected);
    };
    assert_query_round_trip(Query::OtherTracksCount);
    for query in [None, Some(String::new()), Some("Été_100%".into())] {
        for favorite in [None, Some(false), Some(true)] {
            for missing in [None, Some(false), Some(true)] {
                for sort in [LibrarySort::Id, LibrarySort::Album, LibrarySort::MostPlayed] {
                    assert_query_round_trip(Query::OtherTracksLocate {
                        track_id: i64::MAX,
                        query: query.clone(),
                        favorite,
                        missing,
                        sort,
                    });
                    for offset in [0, usize::MAX] {
                        assert_query_round_trip(Query::OtherTracksPage {
                            query: query.clone(),
                            favorite,
                            missing,
                            sort,
                            offset,
                            limit: usize::MAX,
                        });
                    }
                }
            }
        }
    }
    let mut assert_command_round_trip = |command: Command| {
        let expected = serde_json::to_value(&command).unwrap();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(command)),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected command")
        };
        assert_eq!(
            serde_json::to_value(Command::try_from(wire).unwrap()).unwrap(),
            expected
        );
    };
    for other_tracks in [false, true] {
        assert_command_round_trip(Command::CheckOtherTracks {
            force: other_tracks,
        });
        assert_command_round_trip(Command::EnqueueSources {
            directories: vec![],
            track_ids: vec![],
            other_tracks,
        });
        assert_command_round_trip(Command::AddPlaylistSources {
            playlist_id: i64::MAX,
            directories: vec![],
            track_ids: vec![],
            other_tracks,
        });
        for index in [0, usize::MAX] {
            for expected_revision in [0, u64::MAX] {
                assert_command_round_trip(Command::InsertQueueSources {
                    directories: vec!["/music".into()],
                    track_ids: vec![1, i64::MAX],
                    other_tracks,
                    index,
                    expected_revision,
                });
                assert_command_round_trip(Command::InsertPlaylistSources {
                    playlist_id: i64::MAX,
                    directories: vec![],
                    track_ids: vec![],
                    other_tracks,
                    index,
                    expected_revision,
                });
            }
        }
    }
    for index in [None, Some(0), Some(usize::MAX)] {
        for expected_revision in [None, Some(0), Some(u64::MAX)] {
            for paths in [vec!["/音楽/track.wav".into(), "/music".into()], vec![]] {
                assert_command_round_trip(Command::QueueExternalPaths {
                    paths,
                    index,
                    expected_revision,
                });
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
fn playlist_catalog_move_requests_round_trip_ids_anchors_and_revision_boundaries() {
    let mut encoded = Vec::new();
    for playlist_id in [1, i64::MAX] {
        for before_playlist_id in [None, Some(1), Some(i64::MAX)] {
            for after in [false, true] {
                for expected_revision in [0, u64::MAX] {
                    let command = Command::MovePlaylist {
                        playlist_id,
                        before_playlist_id,
                        after,
                        expected_revision,
                    };
                    let expected = serde_json::to_value(&command).unwrap();
                    encode_request_frame_into(&WireRequest::Command(command.into()), &mut encoded)
                        .unwrap();
                    let WireRequest::Command(wire) = decode_request_frame(&encoded[4..]).unwrap()
                    else {
                        panic!("expected catalog move command");
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
        Command::InsertPlaylist {
            playlist_id: 10,
            track_ids: vec![3, 1, 3],
            index: 7,
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
    assert_eq!(encoded[5], ResponseTag::Ack as u8);
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
            configuration: 5,
            processing: 6,
        },
        playback: None,
        queue: None,
        library: Some(rivu_protocol::model::LibrarySnapshot::default()),
        system: None,
        configuration: None,
        processing: None,
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
    assert!(state.configuration.is_none());
}

#[test]
fn runtime_wire_round_trip_preserves_large_client_configuration_without_transferring_it() {
    let mut snapshot = rivu_protocol::model::ClientSnapshot::default();
    let config = Arc::make_mut(&mut snapshot.configuration.config);
    config.library_roots = (0..256)
        .map(|index| {
            PathBuf::from(format!(
                "/configuration-only-marker/{index}/{}",
                "long-root-".repeat(20)
            ))
        })
        .collect();
    config.query_batch_size = 91;
    snapshot.configuration.config_path = "/configuration-only-marker/config.toml".into();
    let retained = Arc::clone(&snapshot.configuration.config);
    let full = StateResponse {
        system: Some(snapshot.system.clone()),
        configuration: Some(snapshot.configuration.clone()),
        ..Default::default()
    };
    let mut encoded_full = Vec::new();
    encode_response_frame_into(&WireResponse::State(wire_state(full)), &mut encoded_full).unwrap();
    assert!(
        encoded_full
            .windows(b"configuration-only-marker".len())
            .any(|bytes| bytes == b"configuration-only-marker")
    );

    let runtime = StateResponse {
        system: Some(rivu_protocol::model::SystemState {
            scan_message: "runtime progress".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::State(wire_state(runtime)), &mut encoded).unwrap();
    assert!(
        !encoded
            .windows(b"configuration-only-marker".len())
            .any(|bytes| bytes == b"configuration-only-marker")
    );
    let WireResponse::State(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected state")
    };
    let response = state_response(decoded);
    assert_eq!(
        response.sections(),
        StateSections {
            system: true,
            ..Default::default()
        }
    );
    let mut observed = crate::response::ObservedState::new(1);
    observed.apply(
        1,
        StateResponse {
            configuration: Some(snapshot.configuration),
            ..Default::default()
        },
    );
    observed.apply(1, response);
    let snapshot = observed.snapshot();
    assert_eq!(snapshot.system.scan_message, "runtime progress");
    assert!(Arc::ptr_eq(&retained, &snapshot.configuration.config));
    assert_eq!(snapshot.configuration.config.query_batch_size, 91);
    assert_eq!(
        snapshot.configuration.config_path,
        std::path::Path::new("/configuration-only-marker/config.toml")
    );
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
                configuration: None,
                processing: None,
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
fn configuration_wire_adapter_preserves_optional_fields_and_library_columns() {
    for (output_device, library_columns) in [
        (
            None,
            rivu_format::config::LibraryColumns {
                duration: false,
                ..Default::default()
            },
        ),
        (
            Some("USB audio output".to_owned()),
            rivu_format::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        let config = Arc::new(rivu_format::config::Config {
            scan_min_duration_seconds: if output_device.is_some() { u32::MAX } else { 0 },
            visualization_palette: rivu_format::config::VisualizationPalette::Viridis,
            output_device,
            library_columns,
            ui_theme: rivu_format::config::Theme::TokyoNight,
            artwork_style: rivu_format::config::ArtworkStyle::Metal,
            artwork_mode: rivu_format::config::ArtworkMode::Rotating,
            artwork_period_seconds: 12.5,
            artwork_direction: rivu_format::config::ArtworkDirection::Counterclockwise,
            animation_fps: 240,
            ..Default::default()
        });
        let configuration = rivu_protocol::model::ConfigurationState {
            config: config.clone(),
            config_path: "/test/config.toml".into(),
        };
        let mut encoded = Vec::new();
        encode_response_frame_into(
            &WireResponse::State(wire_state(StateResponse {
                revisions: StateRevisions::default(),
                playback: None,
                queue: None,
                library: None,
                system: None,
                configuration: Some(configuration),
                processing: None,
            })),
            &mut encoded,
        )
        .unwrap();
        let response = decode_response_frame(&encoded[4..]).unwrap();
        let WireResponse::State(state) = response else {
            panic!("expected state")
        };
        assert_eq!(
            state_response(state).configuration.unwrap().config.as_ref(),
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
            expected_revision: 12,
        })),
        WireRequest::Watch(
            StateRevisions {
                playback: 1,
                queue: 2,
                library: 3,
                system: 4,
                configuration: 5,
                processing: 6,
            }
            .into(),
        ),
        WireRequest::WaitTask(rivu_protocol::model::TaskTarget::Scan { id: 42 }),
        WireRequest::QueryRetained(rivu_protocol::wire::WireRetainedQuery {
            id: 73,
            query: WireQuery::Track { track_id: 42 },
        }),
        WireRequest::RegisterDesktop,
        WireRequest::QueryCancel(73),
    ];
    for request in requests {
        let expected_type = match &request {
            WireRequest::Hello(_) => RequestTag::Hello,
            WireRequest::State(_) => RequestTag::State,
            WireRequest::Query(_) => RequestTag::Query,
            WireRequest::Command(_) => RequestTag::Command,
            WireRequest::Watch(_) => RequestTag::Watch,
            WireRequest::WaitTask(_) => RequestTag::WaitTask,
            WireRequest::QueryRetained(_) => RequestTag::QueryRetained,
            WireRequest::RegisterDesktop => RequestTag::RegisterDesktop,
            WireRequest::QueryCancel(_) => RequestTag::QueryCancel,
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
            WireRequest::QueryRetained(query) => {
                assert_eq!(query.id, 73);
                assert!(matches!(query.query, WireQuery::Track { track_id: 42 }));
            }
            WireRequest::RegisterDesktop => {}
            WireRequest::QueryCancel(id) => assert_eq!(id, 73),
            WireRequest::WaitTask(target) => {
                assert_eq!(target, rivu_protocol::model::TaskTarget::Scan { id: 42 })
            }
            WireRequest::Command(WireCommand::MoveQueue {
                queue_id,
                index,
                expected_revision,
            }) => {
                assert_eq!((queue_id, index, expected_revision), (4, 9, 12))
            }
            WireRequest::Watch(watch) => {
                assert_eq!(watch.sections, StateSections::ALL);
                assert_eq!(
                    watch.revisions,
                    StateRevisions {
                        playback: 1,
                        queue: 2,
                        library: 3,
                        system: 4,
                        configuration: 5,
                        processing: 6,
                    }
                );
            }
            _ => panic!("unexpected request variant"),
        }
    }
}

#[test]
fn configure_request_round_trip_preserves_optional_config_field_and_library_columns() {
    for (output_device, library_columns) in [
        (
            None,
            rivu_format::config::LibraryColumns {
                duration: false,
                ..Default::default()
            },
        ),
        (
            Some("USB audio output".to_owned()),
            rivu_format::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        let config = rivu_format::config::Config {
            default_visualizer_reference_lines: output_device.is_some(),
            default_visualizer_labels: output_device.is_none(),
            oscilloscope_overlay_channels: output_device.is_some(),
            scan_min_duration_seconds: if output_device.is_some() { u32::MAX } else { 0 },
            visualization_palette: rivu_format::config::VisualizationPalette::Magma,
            output_device,
            library_columns,
            ui_theme: rivu_format::config::Theme::TokyoNightDay,
            artwork_style: rivu_format::config::ArtworkStyle::Duotone,
            artwork_mode: rivu_format::config::ArtworkMode::Rotating,
            artwork_period_seconds: 12.5,
            artwork_direction: rivu_format::config::ArtworkDirection::Counterclockwise,
            animation_fps: 30,
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
fn set_play_count_wire_round_trip_preserves_integer_boundaries() {
    for track_id in [i64::MIN, 1, i64::MAX] {
        for play_count in [0, i64::MAX as u64, u64::MAX] {
            let request = WireRequest::Command(WireCommand::from(Command::SetPlayCount {
                track_id,
                play_count,
            }));
            let mut encoded = Vec::new();
            encode_request_frame_into(&request, &mut encoded).unwrap();
            let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
                panic!("expected command request");
            };
            let Command::SetPlayCount {
                track_id: decoded_id,
                play_count: decoded_count,
            } = command.try_into().unwrap()
            else {
                panic!("expected play count command");
            };
            assert_eq!((decoded_id, decoded_count), (track_id, play_count));
        }
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
    assert_eq!(encoded, [8, 0, 0, 0, PROTOCOL_VERSION, 0, 1, 1, 1, 1, 1, 1]);
    encode_response_frame_into(&WireResponse::Error("x".into()), &mut encoded).unwrap();
    assert_eq!(encoded, [4, 0, 0, 0, PROTOCOL_VERSION, 255, 1, b'x']);
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
        let mut encoded = vec![PROTOCOL_VERSION, ResponseTag::Error as u8];
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
    let mut malformed = vec![PROTOCOL_VERSION, ResponseTag::Error as u8];
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
fn direct_decoders_enforce_frame_limits_before_body_decode() {
    let mut encoded = vec![0xff; MAX_REQUEST + 1];
    encoded[0] = PROTOCOL_VERSION;
    encoded[1] = RequestTag::Command as u8;
    let error = decode_request_frame(&encoded).err().unwrap();
    assert!(error.downcast_ref::<postcard::Error>().is_none());
    encoded.resize(MAX_RESPONSE + 1, 0xff);
    encoded[1] = ResponseTag::Error as u8;
    let error = decode_response_frame(&encoded).err().unwrap();
    assert!(error.downcast_ref::<postcard::Error>().is_none());
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
fn large_explicit_occurrence_selections_fit_bounded_request_frames() {
    let mut encoded = Vec::new();
    encode_request_frame_into(
        &WireRequest::Command(
            Command::MoveQueueEntries {
                queue_ids: (1..=200_000).collect(),
                index: 0,
                expected_revision: 1,
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
fn preset_write_intent_is_explicit_in_the_current_wire_contract() {
    use rivu_protocol::model::EqualizerEdit;
    for overwrite in [false, true] {
        let save = EqualizerEdit::SavePreset {
            name: "x".to_owned(),
            overwrite,
        };
        assert_eq!(
            postcard::to_allocvec(&save).unwrap(),
            [9, 1, b'x', u8::from(overwrite)]
        );
    }
    for json in [
        r#"{"SavePreset":{"name":"x"}}"#,
        r#"{"ImportPreset":{"name":"x","path":"preset.json"}}"#,
    ] {
        let edit: EqualizerEdit = serde_json::from_str(json).unwrap();
        assert!(matches!(
            edit,
            EqualizerEdit::SavePreset {
                overwrite: false,
                ..
            } | EqualizerEdit::ImportPreset {
                overwrite: false,
                ..
            }
        ));
    }
}

#[test]
fn processing_commands_and_configuration_use_current_positional_contract() {
    use rivu_protocol::model::{
        DenoisePatch, EqBandEdit, EqualizerEdit, NeuralPatch, NoiseSelection, ProcessingOperation,
    };
    let patch: DenoisePatch = serde_json::from_str(r#"{"spectral_gain_floor_db":-18.5}"#).unwrap();
    assert_eq!(patch.spectral_gain_floor_db, Some(-18.5));
    // The spectral gain floor is the third positional field; absent controls
    // retain their positions and no named key is sent over IPC.
    let mut expected = vec![0, 0, 1];
    expected.extend_from_slice(&(-18.5_f32).to_le_bytes());
    expected.resize(23, 0);
    assert_eq!(postcard::to_allocvec(&patch).unwrap(), expected);
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
            expected_revision: 37,
            edit: EqualizerEdit::BandField {
                index: 2,
                field: EqBandEdit::Position {
                    frequency_hz: 1200.0,
                    gain_db: 4.0,
                },
            },
        },
        Command::DenoiseEdit {
            patch: Box::new(patch),
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
    let mut config = rivu_format::config::Config {
        log_max_files: 128,
        ..Default::default()
    };
    config.equalizer.enabled = true;
    config
        .equalizer
        .bands
        .push(rivu_dsp::equalizer::config::EqBand::default());
    config.denoise.spectral_gain_floor_db = -31.5;
    config.neural.speech_manifest = Some("/models/speech.json".into());
    config.neural.separation_overlap = 0.5;
    config.material.enabled = false;
    config.material.selected = rivu_format::config::MaterialPreset::StillWater;
    config.material.presets.insert(
        rivu_format::config::MaterialPreset::StillWater,
        rivu_format::config::MaterialParameters::preset(
            rivu_format::config::MaterialPreset::StillWater,
        ),
    );
    let config = Arc::new(config);
    let wire = WireConfig::from(Arc::clone(&config));
    let bytes = postcard::to_allocvec(&wire).unwrap();
    let decoded: WireConfig = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(rivu_format::config::Config::from(decoded), *config);
}

#[test]
fn shared_configuration_encoding_rejects_oversize_and_recovers() {
    let config = Arc::new(rivu_format::config::Config {
        ui_font: "x".repeat(MAX_RESPONSE),
        ..Default::default()
    });
    let mut encoded = Vec::new();
    let request = WireRequest::Command(WireCommand::from(Command::Configure {
        config: Arc::clone(&config),
    }));
    assert!(encode_request_frame_into(&request, &mut encoded).is_err());
    assert!(encoded.is_empty());
    assert!(encoded.capacity() <= RETAINED_BUFFER_LIMIT);
    let response = WireResponse::State(wire_state(StateResponse {
        configuration: Some(crate::model::ConfigurationState {
            config,
            config_path: "/test/config.toml".into(),
        }),
        ..Default::default()
    }));
    assert!(encode_response_frame_into(&response, &mut encoded).is_err());
    assert!(encoded.is_empty());
    assert!(encoded.capacity() <= RETAINED_BUFFER_LIMIT);

    let config = Arc::new(rivu_format::config::Config::default());
    let request = WireRequest::Command(WireCommand::from(Command::Configure {
        config: Arc::clone(&config),
    }));
    encode_request_frame_into(&request, &mut encoded).unwrap();
    let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("expected command");
    };
    let Command::Configure { config: decoded } = Command::try_from(command).unwrap() else {
        panic!("expected configuration");
    };
    assert_eq!(*decoded, *config);
}

#[test]
fn spectrum_bar_layout_postcard_round_trip_preserves_both_positional_variants() {
    use rivu_format::config::{Config, SpectrumBarLayout};

    for (layout, expected_bytes) in [
        (
            SpectrumBarLayout::Automatic { width: 7.25 },
            vec![0, 0, 0, 232, 64],
        ),
        (SpectrumBarLayout::Fixed { count: 257 }, vec![1, 129, 2]),
    ] {
        let bytes = postcard::to_allocvec(&WireSpectrumBarLayout::from(layout)).unwrap();
        assert_eq!(bytes, expected_bytes);
        let decoded: WireSpectrumBarLayout = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(SpectrumBarLayout::from(decoded), layout);

        let config = Config {
            spectrum_bar_layout: layout,
            spectrum_gap: 2.5,
            spectrogram_history_seconds: 37,
            ..Default::default()
        };
        let config = Arc::new(config);
        let bytes = postcard::to_allocvec(&WireConfig::from(Arc::clone(&config))).unwrap();
        let decoded: WireConfig = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(Config::from(decoded), *config);
    }
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
    let mut config = rivu_format::config::Config::default();
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
            graph_requested_mode: Some(GraphExecutionMode::Auto),
            graph_mode: Some(GraphExecutionMode::Offline),
            graph_boundary: Some(GraphPort { node: 1, port: 0 }),
            graph_export_path: Some("/rendered outputs/voice -6 dB.wav".into()),
        },
        graph_revision: Some(0),
        graph_run_id: Some(90),
        graph_actions_available: true,
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
    assert_eq!(json["graph_run_id"], 90);
    assert_eq!(json["graph_actions_available"], true);
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

    let mut snapshot = crate::model::ClientSnapshot {
        processing: Some(Arc::new(task.clone())),
        ..Default::default()
    };
    let previous = snapshot.clone();
    Arc::make_mut(snapshot.processing.as_mut().unwrap()).phase = ProcessingPhase::Cancelled;
    assert_eq!(
        previous.processing.as_ref().unwrap().phase,
        ProcessingPhase::Complete
    );
    assert_eq!(serde_json::to_value(&previous.processing).unwrap(), json);
    // A system-only response preserves an independently applied processing task.
    encode_response_frame_into(
        &WireResponse::State(wire_state(StateResponse {
            system: Some(snapshot.system.clone()),
            ..Default::default()
        })),
        &mut encoded,
    )
    .unwrap();
    let WireResponse::State(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected system-only state");
    };
    let system_only = state_response(decoded);
    assert!(system_only.processing.is_none());
    let mut observed = crate::response::ObservedState::new(1);
    observed.apply(
        1,
        StateResponse {
            processing: Some(crate::response::ProcessingState {
                task: snapshot.processing.clone(),
            }),
            ..Default::default()
        },
    );
    observed.apply(1, system_only);
    let snapshot = observed.snapshot();
    assert_eq!(
        snapshot.processing.as_ref().unwrap().phase,
        ProcessingPhase::Cancelled
    );

    encode_response_frame_into(
        &WireResponse::State(wire_state(StateResponse {
            processing: Some(crate::response::ProcessingState {
                task: snapshot.processing.clone(),
            }),
            ..StateResponse::default()
        })),
        &mut encoded,
    )
    .unwrap();
    let WireResponse::State(decoded) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected shared processing response");
    };
    let decoded_task = state_response(decoded).processing.unwrap().task.unwrap();
    assert_eq!(decoded_task.phase, ProcessingPhase::Cancelled);
    assert!(decoded_task.graph_actions_available);
    assert_eq!(decoded_task.graph_run_id, Some(90));
    assert_eq!(
        decoded_task.as_ref(),
        snapshot.processing.as_deref().unwrap()
    );
    observed.apply(
        1,
        StateResponse {
            processing: Some(crate::response::ProcessingState { task: None }),
            ..Default::default()
        },
    );
    assert!(observed.snapshot().processing.is_none());
}

#[test]
fn settings_patch_round_trip_preserves_untouched_owner_state_and_explicit_clears() {
    use crate::model::{ConfigMutation, ConfigPatch};
    use rivu_format::config::{Config, SpectrumBarLayout};
    let baseline = Config {
        output_device: Some("draft-output".into()),
        spectrum_db_range: Some(60.0),
        ..Config::default()
    };
    let mut draft = baseline.clone();
    draft.output_device = None;
    draft.spectrum_db_range = None;
    draft.spectrum_bar_layout = SpectrumBarLayout::Fixed { count: 48 };
    draft.scan_max_depth = 7;
    draft.material.enabled = false;
    draft.material.selected = rivu_format::config::MaterialPreset::Dissolve;
    let patch = ConfigPatch::between(&baseline, &draft);
    let mut encoded = Vec::new();
    encode_request_frame_into(
        &WireRequest::Command(WireCommand::from(Command::MutateConfig {
            change: ConfigMutation::Settings(Box::new(patch)),
        })),
        &mut encoded,
    )
    .unwrap();
    let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
        panic!("expected config mutation");
    };
    let Command::MutateConfig {
        change: ConfigMutation::Settings(patch),
    } = Command::try_from(command).unwrap()
    else {
        panic!("expected Settings patch");
    };
    let mut owner = baseline;
    owner.volume = 0.2;
    owner.shuffle = true;
    patch.apply_to(&mut owner);
    assert_eq!(owner.output_device, None);
    assert_eq!(owner.spectrum_db_range, None);
    assert_eq!(owner.spectrum_bar_layout, draft.spectrum_bar_layout);
    assert_eq!(owner.scan_max_depth, 7);
    assert_eq!(owner.volume, 0.2);
    assert!(owner.shuffle);
    assert_eq!(owner.material, draft.material);
}

#[test]
fn public_playback_and_scan_ownership_round_trip_to_client_snapshot() {
    use crate::model::{MediaTaskOwner, ProcessingAudition};
    let audition = ProcessingAudition {
        id: 17,
        output: None,
        mode: None,
    };
    let response = StateResponse {
        playback: Some(crate::response::PlaybackSnapshot {
            playback: Default::default(),
            current_track: None,
            source_generation: 43,
            processing_audition: Some(audition),
            processing_selection: Some(audition),
        }),
        system: Some(crate::model::SystemState {
            scan_owner: Some(MediaTaskOwner::Playlist),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::State(wire_state(response)), &mut encoded).unwrap();
    let WireResponse::State(response) = decode_response_frame(&encoded[4..]).unwrap() else {
        panic!("expected state");
    };
    let mut observed = crate::response::ObservedState::new(1);
    observed.apply(1, state_response(response));
    let snapshot = observed.snapshot();
    assert_eq!(snapshot.source_generation, 43);
    assert_eq!(snapshot.processing_audition, Some(audition));
    assert_eq!(snapshot.processing_selection, Some(audition));
    assert_eq!(snapshot.system.scan_owner, Some(MediaTaskOwner::Playlist));
}

#[test]
fn quick_config_mutations_round_trip_without_full_config_payloads() {
    use crate::model::ConfigMutation;
    use rivu_format::config::LibraryColumn;
    let mut changes = vec![
        ConfigMutation::ToggleVisualization,
        ConfigMutation::ToggleWaveformSeek,
        ConfigMutation::AddLibraryRoot("/music/夜".into()),
        ConfigMutation::RemoveLibraryRoot("/music/old".into()),
        ConfigMutation::Material(rivu_format::config::MaterialChange::Select(
            rivu_format::config::MaterialPreset::StillWater,
        )),
        ConfigMutation::Material(rivu_format::config::MaterialChange::SetEnabled(false)),
        ConfigMutation::Material(rivu_format::config::MaterialChange::Reset(
            rivu_format::config::MaterialPreset::Dissolve,
        )),
        ConfigMutation::Material(rivu_format::config::MaterialChange::SetParameter {
            preset: rivu_format::config::MaterialPreset::StillWater,
            parameter: rivu_format::config::material::MaterialParameter::Wave,
            value: 8.,
        }),
    ];
    for column in [
        LibraryColumn::Artist,
        LibraryColumn::Album,
        LibraryColumn::Duration,
        LibraryColumn::Codec,
        LibraryColumn::Bitrate,
    ] {
        changes.push(ConfigMutation::ToggleLibraryColumn(column));
    }
    let mut encoded = Vec::new();
    for change in changes {
        let expected = serde_json::to_value(&change).unwrap();
        encode_request_frame_into(
            &WireRequest::Command(WireCommand::from(Command::MutateConfig { change })),
            &mut encoded,
        )
        .unwrap();
        let WireRequest::Command(command) = decode_request_frame(&encoded[4..]).unwrap() else {
            panic!("expected config command");
        };
        let Command::MutateConfig { change } = Command::try_from(command).unwrap() else {
            panic!("expected mutation");
        };
        assert_eq!(serde_json::to_value(change).unwrap(), expected);
    }
}

#[test]
fn public_settings_patch_json_distinguishes_clear_from_unchanged() {
    use crate::model::ConfigPatch;
    let patch: ConfigPatch =
        serde_json::from_value(serde_json::json!({"output_device": null, "scan_max_depth": 7}))
            .unwrap();
    let mut owner = rivu_format::config::Config {
        output_device: Some("owner".into()),
        volume: 0.2,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&patch).unwrap(),
        serde_json::json!({"output_device": null, "scan_max_depth": 7})
    );
    patch.apply_to(&mut owner);
    assert_eq!(owner.output_device, None);
    assert_eq!(owner.scan_max_depth, 7);
    assert_eq!(owner.volume, 0.2);
}
