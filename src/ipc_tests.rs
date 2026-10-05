#[test]
fn wire_revision_wraps_by_equality_domain() {
    let state = AppState {
        revision: u16::MAX as u64,
        ..AppState::default()
    };
    assert_eq!(playback_snapshot(&state).revision, u16::MAX);
    let state = AppState {
        revision: u16::MAX as u64 + 1,
        ..AppState::default()
    };
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
