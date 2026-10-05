#[test]
fn wire_revision_wraps_by_equality_domain() {
    let mut state = ClientSnapshot::default();
    state.system.revision = u16::MAX as u64;
    assert_eq!(playback_snapshot(&state).revision, u16::MAX);
    let mut state = ClientSnapshot::default();
    state.system.revision = u16::MAX as u64 + 1;
    assert_eq!(playback_snapshot(&state).revision, 0);
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
fn full_response_round_trip_preserves_client_snapshot() {
    let response = StateResponse {
        ok: true,
        error: None,
        state: ClientSnapshot::default(),
    };
    let frame = ResponseFrame {
        instance_id: 7,
        revision: 0,
        response: WireResponse::from_response(response, false),
    };
    let encoded = encode_frame(&frame).unwrap();
    let decoded = decode_frame::<ResponseFrame>(&encoded[4..]).unwrap();
    let response = unpack_response(decoded).unwrap();

    assert!(response.ok);
    assert_eq!(response.state.system.revision, 0);
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
