#[test]
fn ack_wire_round_trip_is_independent_from_state() {
    let ack = Ack {
        ok: false,
        error: Some("failed".into()),
        revision: 42,
    };
    let frame = response_frame(WireResponse::Ack(ack.clone()), [7; 16]);
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    assert_eq!(decoded.version, PROTOCOL_VERSION);
    assert_eq!(decoded.instance_id, [7; 16]);
    let WireResponse::Ack(decoded) = decoded.response else {
        panic!("expected ack");
    };
    assert_eq!(decoded.revision, ack.revision);
    assert_eq!(decoded.error, ack.error);
}
#[test]
fn query_wire_response_has_no_state_queue_payload() {
    let response = QueryResponse {
        revisions: crate::response::QueryRevisions::default(),
        result: Err(crate::response::QueryError::Message("query failed".into())),
    };
    let frame = response_frame(WireResponse::Query(response), [3; 16]);
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    assert!(matches!(
        decoded.response,
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
    let frame = response_frame(WireResponse::State(wire_state(response)), [9; 16]);
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    let WireResponse::State(state) = decoded.response else {
        panic!("expected state");
    };
    let state = state_response(state);
    assert_eq!(state.revisions.library, 3);
    assert!(state.library.is_some());
    assert!(state.playback.is_none());
    assert!(state.queue.is_none());
    assert!(state.system.is_none());
}

#[test]
fn system_config_wire_adapter_preserves_optional_fields() {
    let config = Arc::new(crate::config::Config {
        output_device: Some("USB audio output".to_owned()),
        ..Default::default()
    });
    let mut system = crate::model::SystemState::default();
    system.config = config.clone();
    system.revision = 42;
    let frame = response_frame(
        WireResponse::State(wire_state(StateResponse {
            revisions: StateRevisions::default(),
            playback: None,
            queue: None,
            library: None,
            system: Some(system),
        })),
        [1; 16],
    );
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    let WireResponse::State(state) = decoded.response else {
        panic!("expected state");
    };
    let state = state_response(state);
    assert_eq!(state.system.unwrap().config.as_ref(), config.as_ref());
}

#[test]
fn request_round_trip_uses_binary_wire_body() {
    let requests = [
        (
            WireRequest::Query(WireQuery::from(Query::LibraryPage {
                query: Some("album".into()),
                favorite: Some(true),
                missing: Some(false),
                sort: crate::model::LibrarySort::Album,
                offset: 7,
                limit: 11,
            })),
            REQUEST_QUERY,
        ),
        (WireRequest::State(StateSections::ALL), REQUEST_STATE),
        (
            WireRequest::Command(WireCommand::from(Command::MoveQueue {
                queue_id: 4,
                index: 9,
            })),
            REQUEST_COMMAND,
        ),
        (
            WireRequest::Watch(StateRevisions {
                playback: 1,
                queue: 2,
                library: 3,
                system: 4,
            }),
            REQUEST_WATCH,
        ),
    ];
    for (request, kind) in requests {
        let frame = RequestFrame {
            version: PROTOCOL_VERSION,
            instance_id: UNKNOWN_INSTANCE,
            request,
        };
        let mut encoded = Vec::new();
        encode_request_frame_into(&frame, &mut encoded).unwrap();
        assert_eq!(encoded[4 + 2 + 16], kind);
        let decoded = decode_request_frame(&encoded[4..]).unwrap().request;
        match kind {
            REQUEST_QUERY => assert!(matches!(
                decoded,
                WireRequest::Query(WireQuery::LibraryPage {
                    offset: 7,
                    limit: 11,
                    ..
                })
            )),
            REQUEST_STATE => assert!(matches!(decoded, WireRequest::State(StateSections { .. }))),
            REQUEST_COMMAND => assert!(matches!(
                decoded,
                WireRequest::Command(WireCommand::MoveQueue {
                    queue_id: 4,
                    index: 9
                })
            )),
            REQUEST_WATCH => assert!(matches!(
                decoded,
                WireRequest::Watch(StateRevisions {
                    playback: 1,
                    queue: 2,
                    library: 3,
                    system: 4
                })
            )),
            _ => unreachable!(),
        }
    }
}
#[test]
fn request_encoding_rejects_oversized_body_before_completion() {
    let frame = RequestFrame {
        version: PROTOCOL_VERSION,
        instance_id: UNKNOWN_INSTANCE,
        request: WireRequest::Command(WireCommand::from(Command::EditTrack {
            track_id: 1,
            title: "x".repeat(MAX_REQUEST),
            artist: String::new(),
            album: String::new(),
        })),
    };
    let mut encoded = Vec::new();
    assert!(encode_request_frame_into(&frame, &mut encoded).is_err());
}

#[test]
fn invalid_protocol_version_is_rejected_before_request_decode() {
    let frame = RequestFrame {
        version: PROTOCOL_VERSION + 1,
        instance_id: UNKNOWN_INSTANCE,
        request: WireRequest::State(StateSections::ALL),
    };
    let mut encoded = Vec::new();
    encode_request_frame_into(&frame, &mut encoded).unwrap();
    let error = match decode_request_frame(&encoded[4..]) {
        Ok(_) => panic!("invalid version accepted"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("Unsupported IPC protocol version"));
}
#[test]
fn invalid_instance_is_rejected_without_dispatch() {
    let error = validate_instance([1; 16], [2; 16]).unwrap_err();
    assert!(format!("{error:#}").contains("instance changed"));
    assert!(validate_instance(UNKNOWN_INSTANCE, [2; 16]).is_ok());
}

#[test]
fn cancellable_query_stops_before_connect_when_already_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("missing.sock");
    let cancelled = Arc::new(AtomicBool::new(true));
    let notify = Arc::new(tokio::sync::Notify::new());
    let error = query_with_cancel(&socket, &Query::LibraryStats, cancelled, notify).unwrap_err();
    assert!(format!("{error:#}").contains("IPC request cancelled"));
}
