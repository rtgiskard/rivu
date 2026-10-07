#[test]
fn ack_wire_round_trip_has_explicit_response_type() {
    let ack = Ack {
        ok: false,
        error: Some("failed".into()),
        revision: 42,
    };
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::Ack(ack.clone()), &mut encoded).unwrap();
    assert_eq!(encoded[5], ResponseType::Ack as u8);
    let (version, response) = decode_response_frame(&encoded[4..]).unwrap();
    assert_eq!(version, PROTOCOL_VERSION);
    let WireResponse::Ack(decoded) = response else {
        panic!("expected ack")
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
    let mut encoded = Vec::new();
    encode_response_frame_into(&WireResponse::Query(response), &mut encoded).unwrap();
    let (_, response) = decode_response_frame(&encoded[4..]).unwrap();
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
    let (_, response) = decode_response_frame(&encoded[4..]).unwrap();
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
fn system_config_wire_adapter_preserves_optional_fields() {
    for output_device in [None, Some("USB audio output".to_owned())] {
        let config = Arc::new(crate::config::Config {
            output_device,
            ..Default::default()
        });
        let mut system = crate::model::SystemState::default();
        system.config = config.clone();
        system.revision = 42;
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
        let (_, response) = decode_response_frame(&encoded[4..]).unwrap();
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
        let (version, decoded) = decode_request_frame(&encoded[4..]).unwrap();
        assert_eq!(version, PROTOCOL_VERSION);
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
fn configure_request_round_trip_preserves_optional_config_field() {
    for output_device in [None, Some("USB audio output".to_owned())] {
        let config = crate::config::Config {
            output_device,
            ..Default::default()
        };
        let request = WireRequest::Command(WireCommand::from(Command::Configure {
            config: config.clone(),
        }));
        let mut encoded = Vec::new();
        encode_request_frame_into(&request, &mut encoded).unwrap();
        let (_, decoded) = decode_request_frame(&encoded[4..]).unwrap();
        let WireRequest::Command(command) = decoded else {
            panic!("expected command request")
        };
        let Command::Configure { config: decoded } = command.try_into().unwrap() else {
            panic!("expected configure command")
        };
        assert_eq!(decoded, config);
    }
}

#[test]
fn request_encoding_rejects_oversized_body_before_completion() {
    let request = WireRequest::Command(WireCommand::from(Command::EditTrack {
        track_id: 1,
        title: "x".repeat(MAX_REQUEST),
        artist: String::new(),
        album: String::new(),
    }));
    let mut encoded = Vec::new();
    assert!(encode_request_frame_into(&request, &mut encoded).is_err());
}

#[test]
fn invalid_protocol_version_is_available_for_dispatch_rejection() {
    let request = WireRequest::State(StateSections::ALL);
    let mut encoded = Vec::new();
    encode_request_frame_into(&request, &mut encoded).unwrap();
    encoded[4] = PROTOCOL_VERSION + 1;
    let (version, _) = decode_request_frame(&encoded[4..]).unwrap();
    assert_eq!(version, PROTOCOL_VERSION + 1);
}

#[test]
fn unknown_message_types_are_rejected_by_explicit_enums() {
    assert!(RequestType::try_from(99).is_err());
    assert!(ResponseType::try_from(99).is_err());
}

#[test]
fn truncated_header_is_rejected() {
    assert!(decode_message_header(&[PROTOCOL_VERSION]).is_err());
}

#[test]
fn unknown_wire_type_is_rejected_before_body_decode() {
    assert!(decode_request_frame(&[PROTOCOL_VERSION, 99]).is_err());
    assert!(decode_response_frame(&[PROTOCOL_VERSION, 99]).is_err());
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
