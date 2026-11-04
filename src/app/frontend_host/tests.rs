use crate::test_support::{
    frontend::{FrontendFixture, save_config},
    ipc::socket_test_directory,
};
use futures_lite::{future, io::AsyncWriteExt};
use rivu_protocol::{
    response::{StateResponse, StateSections},
    transport::{Elapsed, encode_response_frame_into, read_request_frame, timeout},
    wire::{HelloResponse, PROTOCOL_VERSION, ResponseTag, WireRequest, WireResponse, wire_state},
};
use std::{
    cell::Cell,
    os::unix::net::UnixListener,
    time::{Duration, Instant},
};

fn hello_frame(instance_id: u32) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_response_frame_into(
        &WireResponse::Hello(HelloResponse {
            instance_id: instance_id.to_le_bytes(),
            state: wire_state(StateResponse::default()),
        }),
        &mut frame,
    )
    .unwrap();
    frame
}

// Keep the peer connected after its reply, so an incomplete Hello must time out
// rather than ending early with EOF.
fn probe_with_reply(reply: &[u8], budget: Duration) -> anyhow::Result<u32> {
    let directory = socket_test_directory();
    let listener = async_io::Async::new(
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap(),
    )
    .unwrap();
    let received_hello = Cell::new(false);
    let result = async_io::block_on(async {
        let deadline = Instant::now() + budget;
        timeout(
            budget + Duration::from_millis(500),
            future::or(super::probe(directory.path(), deadline), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request_frame(&mut stream, &mut bytes::BytesMut::new())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(request, WireRequest::Hello(_)));
                received_hello.set(true);
                stream.write_all(reply).await.unwrap();
                future::pending().await
            }),
        )
        .await
        .expect("probe exceeded its deadline and watchdog allowance")
    });
    assert!(received_hello.get(), "probe never sent its Hello request");
    result
}

#[test]
fn ready_service_does_not_validate_unused_frontend_config() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = socket_test_directory();
    let active_config = directory.path().join("config.toml");
    save_config(
        &crate::config::Config {
            media_read_buffer_mib: 1,
            ffmpeg_enabled: false,
            ..Default::default()
        },
        &active_config,
    )
    .unwrap();
    let _service = FrontendFixture::start(directory.path(), &active_config).unwrap();
    let original_active = std::fs::read(&active_config).unwrap();
    let mut session = rivu_client::client_session(directory.path()).unwrap();
    session.get_state(StateSections::default()).unwrap();
    let expected = session.instance_id().unwrap();
    let unused_config = directory
        .path()
        .join(OsString::from_vec(b"unused-\xff.toml".to_vec()));
    let original = b"intentionally not valid TOML";
    std::fs::write(&unused_config, original).unwrap();

    assert_eq!(
        super::ensure_service(directory.path(), &unused_config).unwrap(),
        expected
    );
    assert_eq!(std::fs::read(&unused_config).unwrap(), original);
    let missing_config = directory.path().join("unused-missing.toml");
    assert_eq!(
        super::ensure_service(directory.path(), &missing_config).unwrap(),
        expected
    );
    assert!(!missing_config.exists());
    assert_eq!(std::fs::read(&active_config).unwrap(), original_active);
}

#[test]
fn probe_deadline_covers_silent_socket_and_partial_hello_frames() {
    let frame = hello_frame(7);
    for reply in [&[][..], &frame[..2], &frame[..frame.len() - 1]] {
        let error = probe_with_reply(reply, Duration::from_millis(250)).unwrap_err();
        assert!(
            error.downcast_ref::<Elapsed>().is_some(),
            "partial response of {} bytes did not end at the probe deadline: {error:#}",
            reply.len()
        );
    }
}

#[test]
fn probe_rejects_invalid_handshakes_without_losing_the_protocol_error() {
    let mut incompatible = hello_frame(7);
    incompatible[4] = PROTOCOL_VERSION.wrapping_add(1);
    let malformed = vec![2, 0, 0, 0, PROTOCOL_VERSION, ResponseTag::Hello as u8];
    let mut wrong_response = Vec::new();
    encode_response_frame_into(
        &WireResponse::State(wire_state(StateResponse::default())),
        &mut wrong_response,
    )
    .unwrap();
    for reply in [incompatible, malformed, wrong_response, hello_frame(0)] {
        let error = probe_with_reply(&reply, Duration::from_secs(2)).unwrap_err();
        assert!(
            error.downcast_ref::<Elapsed>().is_none(),
            "invalid handshake waited for the deadline instead of being rejected: {error:#}"
        );
    }
}
