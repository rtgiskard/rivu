use super::*;
use crate::terminal::{
    paged::PagedList,
    state::{UiState, View},
};
use rivu::test_support::ipc;
use rivu_protocol::model::Command;
use rivu_protocol::query_tracking::QueryTracker;
use rivu_protocol::response::QueryRevisions;
use std::time::Duration;

fn assert_query_socket_closed(mut stream: std::os::unix::net::UnixStream) {
    use std::io::Read;
    let mut received = Vec::new();
    stream.read_to_end(&mut received).unwrap();
    let mut pending = received.as_slice();
    while !pending.is_empty() {
        let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
        assert!(matches!(
            rivu_protocol::transport::decode_request_frame(&pending[4..4 + length]).unwrap(),
            rivu_protocol::wire::WireRequest::QueryCancel(_)
        ));
        pending = &pending[4 + length..];
    }
}

fn queue_request(
    tracker: &mut QueryTracker,
    offset: usize,
    revisions: QueryRevisions,
) -> QueryRequest {
    let query = ReadKind::Queue.into_query(offset, 20);
    let submission = tracker.submit(&query, revisions);
    QueryRequest {
        demand: Arc::new(ReadSpec {
            offset,
            limit: 20,
            stamp: query.stamp(revisions),
            query,
            target: ReadTarget::Queue,
            filter: None,
        }),
        submission,
    }
}

fn injected_watcher(update: Option<WatchedSnapshot>) -> (Watcher, Sender<()>) {
    let (sender, receiver) = bounded(1);
    let watcher = Watcher {
        receiver,
        latest: Arc::new(Mutex::new(update)),
        stopping: rivu_client::SessionCancellation::default(),
        worker: None,
    };
    (watcher, sender)
}

#[test]
fn shutdown_snapshot_exits_loading_before_input_or_worker_dispatch() {
    use ratatui::{Terminal, backend::TestBackend};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut state = ClientSnapshot::default();
    state.system.shutting_down = true;
    state.queue.total = 1;
    let (watcher, sender) = injected_watcher(Some(Ok((1, state))));
    // The shutdown snapshot remains authoritative after its publisher exits.
    drop(sender);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    crate::terminal::run_loop_with_events(
        directory.path(),
        &mut terminal,
        Arc::new(rivu_presentation::i18n::Localizer::for_terminal("en")),
        &AtomicBool::new(false),
        || watcher.take_update(),
        |_| panic!("shutdown must not wait for keyboard input"),
    )
    .unwrap();
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn shutdown_snapshot_exits_existing_projection_and_cancels_pending_query() {
    use ipc::{complete_test_handshake, read_test_query};
    use ratatui::{Terminal, backend::TestBackend};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(matches!(
            read_test_query(&mut stream),
            Query::QueuePage { .. }
        ));
        started.send(stream).unwrap();
    });
    let mut state = ClientSnapshot::default();
    state.queue.total = 1;
    let (watcher, sender) = injected_watcher(Some(Ok((1, state.clone()))));
    let latest = Arc::clone(&watcher.latest);
    let mut sender = Some(sender);
    let mut pending = None;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    crate::terminal::run_loop_with_events(
        directory.path(),
        &mut terminal,
        Arc::new(rivu_presentation::i18n::Localizer::for_terminal("en")),
        &AtomicBool::new(false),
        || watcher.take_update(),
        |_| {
            assert!(
                pending.is_none(),
                "shutdown must exit before another input wait"
            );
            pending = Some(active.recv_timeout(Duration::from_secs(5)).unwrap());
            state.system.shutting_down = true;
            *latest.lock() = Some(Ok((1, state.clone())));
            drop(sender.take());
            Ok(None)
        },
    )
    .unwrap();
    // The owner joined its query worker; no reply, key, or server poll unblocks it.
    assert_query_socket_closed(pending.unwrap());
    server.join().unwrap();
}

#[test]
fn watcher_failure_and_disconnection_exit_without_polling_and_preserve_the_cause() {
    use ratatui::{Terminal, backend::TestBackend};

    for (initialized, failure) in [
        (false, None),
        (true, None),
        (false, Some("state read failed")),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let update = failure
            .map(|error| Err(error.into()))
            .or_else(|| initialized.then(|| Ok((1, ClientSnapshot::default()))));
        let (watcher, sender) = injected_watcher(update);
        let mut sender = Some(sender);
        if !initialized {
            drop(sender.take());
        }
        let mut input_waits = 0;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let error = crate::terminal::run_loop_with_events(
            directory.path(),
            &mut terminal,
            Arc::new(rivu_presentation::i18n::Localizer::for_terminal("en")),
            &AtomicBool::new(false),
            || watcher.take_update(),
            |_| {
                input_waits += 1;
                assert_eq!(
                    input_waits, 1,
                    "disconnection must not leave an infinite poll"
                );
                drop(sender.take());
                Ok(None)
            },
        )
        .unwrap_err();
        assert_eq!(input_waits, usize::from(initialized));
        if let Some(failure) = failure {
            assert_eq!(error.to_string(), failure);
        }
    }
}

#[test]
fn watcher_shutdown_closes_stalled_initial_handshake() {
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let watcher = spawn_watcher(directory.path());
    let mut stream = listener.accept().unwrap().0;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let length = u32::from_le_bytes(prefix) as usize;
    assert!(length <= 4096);
    stream.read_exact(&mut vec![0; length]).unwrap();
    // Hello has reached the peer, which deliberately never replies. The
    // loading screen's exit must join the watcher without its response timeout.
    let (finished, completion) = bounded(1);
    let shutdown = std::thread::spawn(move || {
        drop(watcher);
        finished.send(()).unwrap();
    });
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    shutdown.join().unwrap();
}

#[test]
fn watcher_reports_initial_connection_failure_to_loading_consumer() {
    let directory = tempfile::tempdir().unwrap();
    let watcher = spawn_watcher(directory.path());
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(watcher.take_update().is_err());
}

#[test]
fn watcher_excludes_processing_reads_and_publications_but_keeps_shutdown() {
    use rivu_protocol::{
        response::{StateResponse, StateRevisions},
        transport::encode_response_frame_into,
        wire::{HelloResponse, WireRequest, WireResponse, wire_state},
    };
    use std::{io::Write, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (continue_watch, continuation) = bounded(1);
    let (ignored, ignored_update) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let reply = |stream: &mut std::os::unix::net::UnixStream, response| {
            let mut frame = Vec::new();
            encode_response_frame_into(&response, &mut frame).unwrap();
            stream.write_all(&frame).unwrap();
        };
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::Hello(sections)
            if sections == TERMINAL_SECTIONS)
        );
        reply(
            &mut stream,
            WireResponse::Hello(HelloResponse {
                instance_id: 1u32.to_le_bytes(),
                state: wire_state(StateResponse::default()),
            }),
        );
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::Watch(watch)
            if watch.sections == TERMINAL_SECTIONS)
        );
        continuation.recv_timeout(Duration::from_secs(5)).unwrap();
        let processing_only = StateRevisions {
            processing: 1,
            ..Default::default()
        };
        // Even a redundant announcement must not cause a state fetch or publication.
        reply(&mut stream, WireResponse::Watch(processing_only));
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::Watch(watch)
            if watch.sections == TERMINAL_SECTIONS)
        );
        ignored.send(()).unwrap();
        continuation.recv_timeout(Duration::from_secs(5)).unwrap();
        let shutdown = StateRevisions {
            system: 1,
            ..processing_only
        };
        reply(&mut stream, WireResponse::Watch(shutdown));
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::State(sections)
            if sections == (StateSections { system: true, ..Default::default() }))
        );
        reply(
            &mut stream,
            WireResponse::State(wire_state(StateResponse {
                revisions: shutdown,
                system: Some(rivu_protocol::model::SystemState {
                    shutting_down: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        );
    });
    let watcher = spawn_watcher(directory.path());
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (_, initial) = watcher.take_update().unwrap().unwrap();
    assert!(initial.processing.is_none());
    continue_watch.send(()).unwrap();
    ignored_update.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(watcher.latest.lock().is_none());
    assert!(watcher.receiver.try_recv().is_err());
    continue_watch.send(()).unwrap();
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (_, shutdown) = watcher.take_update().unwrap().unwrap();
    assert!(shutdown.system.shutting_down);
    assert!(shutdown.processing.is_none());
    server.join().unwrap();
}

#[test]
fn replacing_queries_keeps_one_stalled_socket_until_worker_shutdown() {
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (connected, accepted) = bounded(1);
    let server = std::thread::spawn(move || {
        connected.send(listener.accept().unwrap().0).unwrap();
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let state = TuiSnapshot::default();
    let mut ui = UiState {
        view: View::Playlists(PagedList::default()),
        ..UiState::default()
    };
    let request = ui.query_request(&state).unwrap();
    worker.request(request.clone());
    let mut stream = accepted.recv_timeout(Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    for offset in 1..100 {
        worker.request(QueryRequest {
            demand: Arc::new(ReadSpec {
                offset,
                ..request.demand.as_ref().clone()
            }),
            submission: request.submission,
        });
    }
    // The peer deliberately stalls Hello. Replacements must not abandon
    // the active connection and start more server work behind it.
    let mut buffer = [0; 4096];
    let retained = loop {
        match stream.read(&mut buffer) {
            Ok(0) => break false,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break true;
            }
            Err(error) => panic!("{error}"),
        }
    };
    drop(worker);
    let closed = stream.read(&mut buffer).unwrap();
    server.join().unwrap();
    assert!(
        retained,
        "replacement abandoned an active server connection"
    );
    assert_eq!(closed, 0, "worker shutdown must close a stalled connection");
}

#[test]
fn replaced_bitrate_queries_drain_then_send_only_latest_on_the_same_session() {
    use ipc::{complete_test_handshake, read_test_query, read_test_request, reply_test_query};
    use rivu_protocol::wire::{WireQuery, WireRequest};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let (release, resume) = bounded(1);
    let (cancel_seen, cancellation) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        let WireRequest::QueryRetained(query) = read_test_request(&mut stream) else {
            panic!("expected retained query");
        };
        assert!(matches!(
            query.query,
            WireQuery::QueuePage { offset: 0, .. }
        ));
        started.send(()).unwrap();
        assert!(
            matches!(read_test_request(&mut stream), WireRequest::QueryCancel(id) if id == query.id)
        );
        cancel_seen.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        for offset in [0, 99] {
            if offset != 0 {
                assert!(matches!(
                    read_test_query(&mut stream),
                    Query::QueuePage { offset: 99, .. }
                ));
            }
            reply_test_query(
                &mut stream,
                rivu_protocol::response::QueryResponse {
                    revisions: QueryRevisions::default().with_bitrate_revision(offset as u64),
                    result: Ok(ViewResponse::QueuePage(rivu_protocol::model::Page {
                        total: 100,
                        rows: Vec::new(),
                    })),
                },
            );
        }
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    worker.request(request.clone());
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    for offset in 1..100 {
        worker.request(queue_request(
            &mut tracker,
            offset,
            QueryRevisions::default().with_bitrate_revision(offset as u64),
        ));
    }
    cancellation.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(worker.latest.lock().is_none());
    release.send(()).unwrap();
    // Finish the accepted jobs without shutdown cancellation so the final
    // consumer result can be inspected without polling or timing races.
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.demand.offset, 99);
    assert_eq!(request.demand.stamp.revisions().bitrate, 99);
    assert_eq!(result.as_ref().unwrap().revisions.bitrate, 99);
    assert!(matches!(result.unwrap().view, ViewResponse::QueuePage(page) if page.total == 100));
    server.join().unwrap();
}

#[test]
fn query_worker_shutdown_cancels_active_exchange_and_discards_pending_job() {
    use ipc::{complete_test_handshake, read_test_query};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(matches!(
            read_test_query(&mut stream),
            Query::QueuePage { offset: 0, .. }
        ));
        started.send(stream).unwrap();
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    worker.request(request.clone());
    let stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.request(QueryRequest {
        demand: Arc::new(ReadSpec {
            offset: 99,
            ..request.demand.as_ref().clone()
        }),
        submission: request.submission,
    });
    let pending = Arc::clone(&worker.pending);
    let results = Arc::clone(&worker.latest);
    drop(worker);
    assert!(pending.lock().is_none());
    assert!(results.lock().is_none());
    assert_query_socket_closed(stream);
    server.join().unwrap();
}

#[test]
fn command_worker_reports_ambiguous_failure_without_replaying_the_mutation() {
    use ipc::{complete_test_handshake, read_test_command, reply_test_ack};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
        );
        // Mutation execution is ambiguous to the client: no Ack arrives.
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
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
    let worker = spawn_command_worker(directory.path(), 1);
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(
            Command::CreatePlaylist {
                name: "first".into(),
            }
            .into(),
        )
        .unwrap();
    let failure = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    failure.into_message().unwrap();
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(
            Command::CreatePlaylist {
                name: "second".into(),
            }
            .into(),
        )
        .unwrap();
    let ack = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(ack.is_success());
    drop(worker);
    server.join().unwrap();
}

#[test]
fn command_worker_shutdown_closes_pending_exchange_and_drops_queued_commands() {
    use ipc::{complete_test_handshake, read_test_command};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(
            matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
        );
        started.send(stream).unwrap();
    });
    let worker = spawn_command_worker(directory.path(), 1);
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(
            Command::CreatePlaylist {
                name: "first".into(),
            }
            .into(),
        )
        .unwrap();
    let mut stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(
            Command::CreatePlaylist {
                name: "second".into(),
            }
            .into(),
        )
        .unwrap();
    drop(worker);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    server.join().unwrap();
}

#[test]
fn watcher_identity_binds_delayed_command_and_query_workers() {
    use ipc::{complete_test_handshake, complete_test_handshake_for_instance};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut watch = listener.accept().unwrap().0;
        complete_test_handshake(&mut watch);
        // Keep A's watch alive while lazy workers encounter B. Identical
        // state revision tokens must not authorize this different instance.
        for _ in 0..2 {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake_for_instance(&mut stream, 2);
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        }
    });
    let watcher = spawn_watcher(directory.path());
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (instance_id, _) = watcher.latest.lock().take().unwrap().unwrap();
    assert_eq!(instance_id, 1);
    let commands = spawn_command_worker(directory.path(), instance_id);
    commands
        .sender
        .as_ref()
        .unwrap()
        .send(
            Command::CreatePlaylist {
                name: "A-only".into(),
            }
            .into(),
        )
        .unwrap();
    let failure = commands
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(failure.instance_changed);
    assert!(
        failure.into_message().is_err(),
        "instance failure must leave the TUI loop"
    );
    let mut queries = spawn_query_worker(directory.path(), instance_id);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    queries.request(request);
    queries.sender.take();
    queries.worker.take().unwrap().join().unwrap();
    let (_, result) = queries.latest.lock().take().unwrap();
    let failure = result.err().unwrap();
    assert!(failure.instance_changed);
    assert!(failure.into_message().is_err());
    drop(commands);
    drop(queries);
    drop(watcher);
    server.join().unwrap();
}

#[test]
fn command_worker_reconnection_rejects_replacement_instance() {
    use ipc::{complete_test_handshake, complete_test_handshake_for_instance, read_test_command};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        read_test_command(&mut stream);
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake_for_instance(&mut stream, 2);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let worker = spawn_command_worker(directory.path(), 1);
    for name in ["ambiguous", "old-baseline"] {
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(Command::CreatePlaylist { name: name.into() }.into())
            .unwrap();
        let failure = worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.instance_changed, name == "old-baseline");
    }
    drop(worker);
    server.join().unwrap();
}

#[test]
fn superseded_query_still_reports_replacement_instance_as_fatal() {
    use ipc::complete_test_handshake_for_instance;
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (accepted, connected) = bounded(1);
    let (release, resume) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        accepted.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        complete_test_handshake_for_instance(&mut stream, 2);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    worker.request(queue_request(&mut tracker, 0, QueryRevisions::default()));
    connected.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.cancel();
    release.send(()).unwrap();
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (_, result) = worker.latest.lock().take().unwrap();
    let failure = result.err().unwrap();
    assert!(failure.instance_changed);
    assert!(failure.into_message().is_err());
    server.join().unwrap();
}

#[test]
fn query_worker_reconnection_rejects_replacement_before_accepting_results() {
    use ipc::{complete_test_handshake, complete_test_handshake_for_instance, read_test_query};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let (release, resume) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        assert!(matches!(
            read_test_query(&mut stream),
            Query::QueuePage { offset: 0, .. }
        ));
        started.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(stream);
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake_for_instance(&mut stream, 2);
        assert_eq!(
            stream.read(&mut [0]).unwrap(),
            0,
            "B received a query from A's baseline"
        );
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    worker.request(request.clone());
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.request(QueryRequest {
        demand: Arc::new(ReadSpec {
            offset: 20,
            ..request.demand.as_ref().clone()
        }),
        submission: request.submission,
    });
    release.send(()).unwrap();
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.demand.offset, 20);
    let failure = result.err().unwrap();
    assert!(failure.instance_changed);
    assert!(failure.into_message().is_err());
    server.join().unwrap();
}

#[test]
fn command_shutdown_stops_real_core_and_publishes_terminal_watch_state() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let _core =
        rivu::test_support::frontend::FrontendFixture::start(directory.path(), &config).unwrap();
    let watcher = spawn_watcher(directory.path());
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (instance, initial) = watcher.take_update().unwrap().unwrap();
    assert!(!initial.system.shutting_down);
    let worker = spawn_command_worker(directory.path(), instance);
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(AppCommand::Shutdown)
        .unwrap();
    assert!(
        worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .is_success()
    );
    // Only the watch's real shutdown state authorizes the TUI to exit; no
    // command-side revision refresh is required after the successful Ack.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("Core shutdown must reach the watcher");
        let _ = watcher.receiver.recv_timeout(remaining);
        if let Some((observed_instance, state)) = watcher.take_update().unwrap() {
            assert_eq!(observed_instance, instance);
            if state.system.shutting_down {
                break;
            }
        }
    }
}

#[test]
fn rejected_shutdown_remains_usable_and_disconnected_shutdown_is_not_retried() {
    use ipc::{complete_test_handshake, reply_test_ack};
    use rivu_protocol::{
        response::AckStatus,
        transport::decode_request_frame,
        wire::{WireCommand, WireRequest},
    };
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        for rejected in [true, false] {
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).unwrap();
            let length = u32::from_le_bytes(prefix) as usize;
            assert!(length <= rivu_protocol::wire::MAX_REQUEST);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert!(matches!(
                decode_request_frame(&body).unwrap(),
                WireRequest::Command(WireCommand::Shutdown)
            ));
            if rejected {
                reply_test_ack(
                    &mut stream,
                    Ack {
                        status: AckStatus::NotAdmitted,
                        error: Some("shutdown rejected".into()),
                        revision: 0,
                        scan_id: None,
                        relocation_id: None,
                        processing_id: None,
                        database_optimization: None,
                    },
                );
            }
        }
        drop(stream);
        listener
    });
    let worker = spawn_command_worker(directory.path(), 1);
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(AppCommand::Shutdown)
        .unwrap();
    let ack = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(!ack.is_success());
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(AppCommand::Shutdown)
        .unwrap();
    let error = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    error.into_message().unwrap();
    let listener = server.join().unwrap();
    drop(worker);
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn borrowed_read_specs_preserve_absent_and_empty_filters_and_stamp_actual_queries() {
    let mut state = TuiSnapshot::default();
    state.library.revision = 17;
    state.queue.revision = 9;
    for kind in [
        ReadKind::Queue,
        ReadKind::QueueSelection(42),
        ReadKind::PlaylistSelection {
            playlist_id: 3,
            entry_id: 7,
        },
    ] {
        let absent = ReadDemand {
            kind,
            filter: None,
            offset: 0,
            limit: 2,
        };
        let empty = ReadDemand {
            kind,
            filter: Some(""),
            offset: 0,
            limit: 2,
        };
        let absent_spec = absent.own(&state);
        let empty_spec = empty.own(&state);
        assert_eq!(absent_spec.borrowed(), absent);
        assert_eq!(empty_spec.borrowed(), empty);
        assert!(!absent.matches(&empty_spec, &state));
        assert!(!empty.matches(&absent_spec, &state));
        assert_eq!(
            absent_spec.stamp,
            absent_spec.query().stamp(absent_spec.revisions(&state))
        );
        assert_eq!(
            empty_spec.stamp,
            empty_spec.query().stamp(empty_spec.revisions(&state))
        );
        let mut next = state.clone();
        next.library.revision += 1;
        assert!(absent.matches(&absent_spec, &next));
        assert!(!empty.matches(&empty_spec, &next));
    }
}

#[test]
fn repeated_withdrawal_discards_pending_but_retains_active_until_reply() {
    use ipc::{complete_test_handshake, read_test_query, read_test_request, reply_test_query};
    use rivu_protocol::wire::WireRequest;
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&rivu_protocol::wire::address(directory.path()).unwrap()).unwrap();
    let (started, active) = bounded(1);
    let (cancel_seen, cancelled) = bounded(1);
    let (release, resume) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        let WireRequest::QueryRetained(query) = read_test_request(&mut stream) else {
            panic!("expected retained query");
        };
        started.send(()).unwrap();
        assert!(matches!(
            read_test_request(&mut stream),
            WireRequest::QueryCancel(id) if id == query.id
        ));
        cancel_seen.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        let response = || rivu_protocol::response::QueryResponse {
            revisions: QueryRevisions::default(),
            result: Ok(ViewResponse::QueuePage(rivu_protocol::model::Page {
                total: 100,
                rows: Vec::new(),
            })),
        };
        reply_test_query(&mut stream, response());
        // Cancelled pending offset 1 never runs; a fresh demand uses the same
        // physical session only after its cancelled predecessor really retires.
        assert!(matches!(
            read_test_query(&mut stream),
            Query::QueuePage { offset: 2, .. }
        ));
        reply_test_query(&mut stream, response());
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    // Withdrawing an idle worker must leave its next real demand usable.
    worker.cancel();
    worker.cancel();
    worker.request(queue_request(&mut tracker, 0, QueryRevisions::default()));
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.request(queue_request(&mut tracker, 1, QueryRevisions::default()));
    worker.cancel();
    worker.cancel();
    cancelled.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(worker.latest.lock().is_none());
    worker.request(queue_request(&mut tracker, 2, QueryRevisions::default()));
    release.send(()).unwrap();
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.demand.offset, 2);
    assert!(matches!(result.unwrap().view, ViewResponse::QueuePage(page) if page.total == 100));
    server.join().unwrap();
}
