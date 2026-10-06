#[test]
fn wire_revision_wraps_by_equality_domain() {
    assert!(revision_matches(u16::MAX as u64, u16::MAX));
    assert!(revision_matches(u16::MAX as u64 + 1, 0));
    assert!(!revision_matches(u16::MAX as u64 + 1, u16::MAX));
}

#[test]
fn ack_wire_round_trip_does_not_require_full_state() {
    let ack = Ack {
        ok: false,
        error: Some("failed".into()),
        revision: 42,
    };
    let wire = WireResponse::from_ack(ack.clone());
    let frame = ResponseFrame {
        instance_id: 7,
        revision: ack.revision as u16,
        response: wire,
    };
    let decoded = unpack_ack(frame).unwrap();
    assert_eq!(decoded.revision, ack.revision);
    assert_eq!(decoded.error.as_deref(), Some("failed"));
}

#[test]
fn response_round_trip_preserves_snapshot_and_private_view() {
    let mut state = ClientSnapshot::default();
    state.library.track_total = 8193;
    let response = StateResponse {
        ok: true,
        error: None,
        state,
        view: Some(ViewResponse::TrackPage(crate::model::TrackPage {
            total: 4097,
            rows: Vec::new(),
        })),
    };
    let frame = ResponseFrame {
        instance_id: 7,
        revision: 0,
        response: WireResponse::from_response(response),
    };
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    let response = unpack_response(decoded).unwrap();

    assert!(response.ok);
    assert_eq!(response.state.system.revision, 0);
    assert_eq!(response.state.library.track_total, 8193);
    let Some(ViewResponse::TrackPage(page)) = response.view else {
        panic!("expected request-local track page");
    };
    assert_eq!(page.total, 4097);
    assert!(page.rows.is_empty());
}

#[test]
fn response_round_trip_preserves_default_and_selected_output_device() {
    for output_device in [None, Some("USB audio output".to_owned())] {
        let config = Arc::new(crate::config::Config {
            output_device,
            ..Default::default()
        });
        let mut state = ClientSnapshot::default();
        state.system.config = config.clone();
        state.system.ffmpeg_status = "decoder ready".to_owned();
        state.system.revision = 42;
        let frame = response_frame(
            StateResponse {
                ok: true,
                error: None,
                state,
                view: None,
            },
            7,
        );
        let encoded = encode_frame(&frame).unwrap();
        let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
        let response = unpack_response(decoded).unwrap();

        assert_eq!(response.state.system.config.as_ref(), config.as_ref());
        assert_eq!(response.state.system.ffmpeg_status, "decoder ready");
        assert_eq!(response.state.system.revision, 42);
        assert!(response.view.is_none());
    }
}

#[test]
fn ack_request_round_trip_preserves_command_variant() {
    let frame = RequestFrame {
        instance_id: 0,
        request: RequestKind::Ack(serde_json::to_string(&Command::Stop).unwrap()),
    };
    let encoded = encode_frame(&frame).unwrap();
    let frame = decode_frame::<RequestFrame>(&encoded[4..]).unwrap();
    let decoded = decode_request(frame).unwrap();
    assert!(matches!(decoded, DecodedRequest::Ack(Command::Stop)));
}

#[test]
fn cancellable_request_stops_before_connect_when_already_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("missing.sock");
    let cancelled = Arc::new(AtomicBool::new(true));
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let error =
        request_with_cancel(&socket, &Command::Overview, cancelled, cancel_notify).unwrap_err();
    assert!(format!("{error:#}").contains("IPC request cancelled"));
}
