use super::*;
use rivu_protocol::test_support::{read_test_query, reply_test_query};
use rivu_protocol::wire::PROTOCOL_VERSION;

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
    if isolate_socket_test() {
        return;
    }

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
        reply_test_command(
            &mut stream,
            CommandReceipt::from_result(Err(anyhow::anyhow!("rejected")), 3).into(),
        );
        assert!(matches!(
            read_test_query(&mut stream),
            Query::PlaylistSummaries { offset: 20, .. }
        ));
        reply_test_query(
            &mut stream,
            QueryResponse {
                revisions: Default::default(),
                result: Err(QueryError::Message("request unavailable".into())),
            },
        );
        assert!(matches!(
            read_test_query(&mut stream),
            Query::PlaylistSummaries { offset: 20, .. }
        ));
        reply_test_query(&mut stream, playlist_test_response(7));
    });
    let mut session = client_session(directory.path());
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
        .request_command(Command::CreatePlaylist { name: "new".into() })
        .unwrap();
    assert!(!ack.is_success());
    assert_eq!(ack.receipt().unwrap().revision, 3);
    let query = Query::PlaylistSummaries {
        query: None,
        offset: 20,
        limit: 20,
    };
    assert!(matches!(
        session.query(query.clone()).unwrap().result,
        Err(QueryError::Message(_))
    ));
    let response = session.query(query).unwrap();
    assert!(
        matches!(response.result, Ok(rivu_protocol::response::ViewResponse::PlaylistSummaries(page)) if page.total == 7)
    );
    server.join().unwrap();
}

#[test]
fn client_session_discards_partial_failed_response_before_next_explicit_request() {
    if isolate_socket_test() {
        return;
    }

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
            &WireResponse::Command(CommandReceipt::from_result(Ok(()), 1).into()),
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
        reply_test_command(&mut stream, CommandReceipt::from_result(Ok(()), 2).into());
    });
    let mut session = client_session(directory.path());
    session.bind_instance(1).unwrap();
    let error = session
        .request_command(Command::CreatePlaylist {
            name: "first".into(),
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<CommandFailure>().unwrap().status,
        rivu_protocol::response::AckStatus::Unknown
    );
    let ack = session
        .request_command(Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap();
    assert!(ack.is_success());
    assert_eq!(ack.receipt().unwrap().revision, 2);
    server.join().unwrap();
}

#[test]
fn cancelling_client_session_closes_pending_watch_with_partial_response_offered() {
    if isolate_socket_test() {
        return;
    }

    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let mut session = client_session(directory.path());
    let stop = session.cancellation();
    let client = std::thread::spawn(move || {
        session.get_state(StateSections::default()).unwrap();
        session
            .bind_instance(session.instance_id().unwrap())
            .unwrap();
        session.wait_state_until(StateRevisions::default().into())
    });
    let mut stream = listener.accept().unwrap().0;
    complete_test_handshake(&mut stream);
    assert!(matches!(
        read_test_request(&mut stream),
        WireRequest::WaitState(_)
    ));
    // Cancellation with a partial frame offered, whether or not the client has
    // read it yet; buffer release on cancel is
    // `cancelled_exchange_releases_partial_bulk_buffers`.
    stream.write_all(&[10, 0, 0, 0, PROTOCOL_VERSION]).unwrap();
    stop.cancel();
    // The handshake's read timeout bounds this if cancellation never closes the socket.
    let closed = stream.read(&mut [0]);
    assert!(
        matches!(closed, Ok(0))
            || matches!(closed, Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset)
    );
    assert!(client.join().unwrap().unwrap().is_none());
}

#[test]
fn pinned_session_rejects_initial_replacement_before_commands_queries_or_state() {
    if isolate_socket_test() {
        return;
    }

    use std::{io::Read, os::unix::net::UnixListener};
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "old baseline sent an operation to B"
            );
        }
    });
    let mut session = client_session(directory.path());
    session.bind_instance(1).unwrap();
    let error = session
        .request_command(Command::CreatePlaylist {
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
    let error = session
        .query_retained_until(Query::LibraryStats, std::future::ready(()))
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    let error = session.get_state(StateSections::ALL).unwrap_err();
    assert!(error.is::<InstanceChanged>());
    assert_eq!(session.instance_id(), None);
    server.join().unwrap();
}

#[test]
fn pinned_session_rejects_reconnected_instance_without_replaying_failed_mutation() {
    if isolate_socket_test() {
        return;
    }

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
    let mut session = client_session(directory.path());
    session.bind_instance(1).unwrap();
    assert!(
        session
            .request_command(Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    let error = session
        .request_command(Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap_err();
    assert!(error.is::<InstanceChanged>());
    server.join().unwrap();
}

#[test]
fn independent_cli_session_can_explicitly_reconnect_to_a_new_instance() {
    if isolate_socket_test() {
        return;
    }

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
        reply_test_command(&mut stream, CommandReceipt::from_result(Ok(()), 2).into());
    });
    let mut session = client_session(directory.path());
    assert!(
        session
            .request_command(Command::CreatePlaylist {
                name: "first".into()
            })
            .is_err()
    );
    assert_eq!(session.instance_id(), None);
    assert!(
        session
            .request_command(Command::CreatePlaylist {
                name: "second".into()
            })
            .unwrap()
            .is_success()
    );
    assert_eq!(session.instance_id(), Some(2));
    server.join().unwrap();
}

#[test]
fn bounded_query_timeout_disconnects_before_the_next_query() {
    if isolate_socket_test() {
        return;
    }

    use std::{io::Read, os::unix::net::UnixListener};
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let mut session = client_session(directory.path());
    let clock = Arc::new(ResponseClock::default());
    session.response_clock = Some(clock.clone());
    let server = std::thread::spawn(move || {
        let mut abandoned = listener.accept().unwrap().0;
        complete_test_handshake(&mut abandoned);
        assert_eq!(
            read_test_query(&mut abandoned),
            Query::Track { track_id: 1 }
        );
        clock.advance_and_wait_for_poll(
            rivu_protocol::transport::response_timeout(query_wait_timeout())
                + Duration::from_secs(1),
        );
        assert_eq!(abandoned.read(&mut [0; 1]).unwrap(), 0);

        let mut next = listener.accept().unwrap().0;
        complete_test_handshake(&mut next);
        assert_eq!(read_test_query(&mut next), Query::Track { track_id: 2 });
        reply_test_query(
            &mut next,
            QueryResponse {
                revisions: Default::default(),
                result: Ok(rivu_protocol::response::ViewResponse::Track(None)),
            },
        );
    });

    let error = session.query(Query::Track { track_id: 1 }).unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(session.stream.is_none());
    assert!(
        session
            .query(Query::Track { track_id: 2 })
            .unwrap()
            .result
            .is_ok()
    );
    server.join().unwrap();
}

#[test]
fn retained_query_uses_physical_request_and_returns_actual_reply() {
    if isolate_socket_test() {
        return;
    }

    use std::os::unix::net::UnixListener;
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        complete_test_handshake(&mut stream);
        let WireRequest::Query(query) = read_test_request(&mut stream) else {
            panic!("expected physical retained query");
        };
        assert_eq!(Query::try_from(query.query).unwrap(), Query::LibraryStats);
        write_test_response(
            &mut stream,
            WireResponse::Query(QueryResponse {
                revisions: Default::default(),
                result: Err(rivu_protocol::response::QueryError::Message(
                    "actual owner result".into(),
                )),
            }),
        );
    });
    let mut session = client_session(directory.path());
    assert!(
        matches!(session.query_retained(Query::LibraryStats).unwrap().result,
        Err(rivu_protocol::response::QueryError::Message(message)) if message == "actual owner result")
    );
    server.join().unwrap();
}

#[test]
fn cancelling_retained_query_disconnects_its_physical_session() {
    if isolate_socket_test() {
        return;
    }

    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let mut session = client_session(directory.path());
    let stop = session.cancellation();
    let client = std::thread::spawn(move || session.query_retained(Query::LibraryStats));
    let mut stream = listener.accept().unwrap().0;
    complete_test_handshake(&mut stream);
    assert!(matches!(
        read_test_request(&mut stream),
        WireRequest::Query(_)
    ));
    stop.cancel();
    assert!(client.join().unwrap().is_err());
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

#[test]
fn owner_cancel_during_handshake_skips_unsent_query_and_reuses_session() {
    if isolate_socket_test() {
        return;
    }

    use rivu_protocol::{
        query_tracking::QueryEpoch,
        response::{QueryError, ViewResponse},
    };
    use std::os::unix::net::UnixListener;

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let mut owner = QueryEpoch::default();
    let ticket = owner.ticket();
    let mut session = client_session(directory.path());
    let client = std::thread::spawn(move || {
        let first = session
            .query_retained_until(Query::LibraryStats, ticket.cancelled())
            .unwrap();
        let next = session.query(Query::Track { track_id: 42 }).unwrap();
        (first, next)
    });
    let mut stream = listener.accept().unwrap().0;
    let timeout = Some(Duration::from_secs(5));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    assert!(matches!(
        read_test_request(&mut stream),
        WireRequest::Hello(_)
    ));
    owner.cancel();
    write_test_response(
        &mut stream,
        WireResponse::Hello(HelloResponse {
            instance_id: 1_u32.to_le_bytes(),
            state: wire_state(StateResponse {
                revisions: StateRevisions::default(),
                playback: None,
                current_track: None,
                queue: None,
                library: None,
                system: None,
                configuration: None,
                processing: None,
            }),
        }),
    );
    assert!(matches!(
        read_test_request(&mut stream),
        WireRequest::Query(WireReadQuery {
            query: WireQuery::Track { track_id: 42 },
            ..
        })
    ));
    write_test_response(
        &mut stream,
        WireResponse::Query(QueryResponse {
            revisions: Default::default(),
            result: Ok(ViewResponse::Track(None)),
        }),
    );
    let (first, next) = client.join().unwrap();
    assert!(matches!(first.result, Err(QueryError::Cancelled)));
    assert!(matches!(next.result, Ok(ViewResponse::Track(None))));
}

#[test]
fn session_shutdown_disconnects_even_when_retained_query_owner_is_cancelled() {
    if isolate_socket_test() {
        return;
    }

    use std::{io::Read, os::unix::net::UnixListener};

    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let mut session = client_session(directory.path());
    session.get_state(StateSections::default()).unwrap();
    session.cancel();
    assert!(
        session
            .query_retained_until(Query::LibraryStats, std::future::ready(()))
            .is_err()
    );
    assert_eq!(session.instance_id(), None);
    server.join().unwrap();
}

#[test]
fn owner_cancel_sends_identified_control_and_returns_the_physical_reply_before_the_next_request() {
    if isolate_socket_test() {
        return;
    }

    use rivu_protocol::{
        query_tracking::QueryEpoch,
        response::{QueryError, QueryRevisions, ViewResponse},
    };
    use std::os::unix::net::UnixListener;

    // The peer accepts a follow-up keyed by its terminal reply. An early local
    // cancellation cannot supply that revision, even if the next request is
    // buffered and the server doesn't read it until after sending the reply.
    let rolled_back = QueryRevisions {
        library: 7,
        ..Default::default()
    };
    let directory = socket_test_directory();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (admitted, active) = std::sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        complete_test_handshake(&mut stream);
        let WireRequest::Query(query) = read_test_request(&mut stream) else {
            panic!("expected retained query");
        };
        assert!(matches!(query.query, WireQuery::LibraryStats));
        admitted.send(()).unwrap();
        assert!(
            matches!(read_test_request(&mut stream), WireRequest::CancelRead(id) if id == query.id)
        );
        write_test_response(
            &mut stream,
            WireResponse::Query(QueryResponse {
                revisions: rolled_back,
                result: Err(QueryError::Cancelled),
            }),
        );
        assert!(matches!(
            read_test_request(&mut stream),
            WireRequest::Query(WireReadQuery { query: WireQuery::Track { track_id }, .. }) if track_id == rolled_back.library as i64
        ));
        write_test_response(
            &mut stream,
            WireResponse::Query(QueryResponse {
                revisions: Default::default(),
                result: Ok(ViewResponse::Track(None)),
            }),
        );
    });
    let mut owner = QueryEpoch::default();
    let ticket = owner.ticket();
    let mut session = client_session(directory.path());
    let client = std::thread::spawn(move || {
        let first = session
            .query_retained_until(Query::LibraryStats, ticket.cancelled())
            .unwrap();
        let next = session
            .query(Query::Track {
                track_id: i64::try_from(first.revisions.library).unwrap(),
            })
            .unwrap();
        (first, next)
    });
    active.recv_timeout(Duration::from_secs(2)).unwrap();
    owner.cancel();
    let (first, next) = client.join().unwrap();
    assert!(matches!(first.result, Err(QueryError::Cancelled)));
    assert_eq!(first.revisions, rolled_back);
    assert!(matches!(next.result, Ok(ViewResponse::Track(None))));
    server.join().unwrap();
}
