use super::*;
use rivu_protocol::wire::PROTOCOL_VERSION;

fn read_test_query(stream: &mut std::os::unix::net::UnixStream) -> Query {
    let WireRequest::Query(query) = read_test_request(stream) else {
        panic!("expected query")
    };
    Query::try_from(query).unwrap()
}

fn reply_test_query(stream: &mut std::os::unix::net::UnixStream, response: QueryResponse) {
    write_test_response(stream, WireResponse::Query(response));
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
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
fn pinned_session_rejects_initial_replacement_before_commands_queries_or_state() {
    use std::{io::Read, os::unix::net::UnixListener};
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
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
