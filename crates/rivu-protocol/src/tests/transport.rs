use crate as rivu_protocol;
use crate::{transport::*, wire::*};

#[test]
fn incompatible_versions_are_rejected_before_any_body_decode_including_hello() {
    async_io::block_on(async {
        for version in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
            for message_type in [RequestTag::Hello, RequestTag::Command] {
                let error = decode_request_frame(&[version, message_type as u8, 0xff])
                    .err()
                    .unwrap();
                assert!(error.downcast_ref::<postcard::Error>().is_none());
            }
            for message_type in [ResponseTag::Hello, ResponseTag::Error] {
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
        use futures_lite::io::AsyncReadExt;

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
        use futures_lite::io::AsyncWriteExt;
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
fn peer_verification_checks_kernel_credentials_on_both_ends() {
    let directory = tempfile::tempdir().unwrap();
    let address = rivu_protocol::wire::address(directory.path()).unwrap();
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
        assert!(rivu_protocol::wire::verify_peer(&stream).is_ok());
    }
}
