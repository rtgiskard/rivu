use super::*;
use crate::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};
use std::sync::Arc;

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
    let response = decode_response_frame(&encoded[4..]).unwrap();
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
fn configure_request_round_trip_preserves_optional_config_field() {
    for output_device in [None, Some("USB audio output".to_owned())] {
        let config = crate::config::Config {
            radial_spectrum_grid: output_device.is_some(),
            radial_spectrum_labels: output_device.is_none(),
            output_device,
            ..Default::default()
        };
        let request = WireRequest::Command(WireCommand::from(Command::Configure {
            config: config.clone(),
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
    assert!(encoded.len() <= 4 + MAX_REQUEST);
}

#[test]
fn postcard_frame_headers_and_bodies_preserve_wire_contract() {
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    assert_eq!(encoded, [6, 0, 0, 0, 2, 0, 1, 1, 1, 1]);
    encode_response_frame_into(&WireResponse::Error("x".into()), &mut encoded).unwrap();
    assert_eq!(encoded, [4, 0, 0, 0, 2, 255, 1, b'x']);
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
    // Two header bytes, one command discriminant, three string-length bytes.
    let name = "x".repeat(MAX_REQUEST - 6);
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
    assert_eq!(name.len(), MAX_REQUEST - 6);

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
