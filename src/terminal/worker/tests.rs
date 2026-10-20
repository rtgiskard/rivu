use super::*;
use crate::response::QueryRevisions;
use crate::terminal::{PagedList, UiState, View};
use std::time::Duration;

fn injected_watcher(update: Option<WatchedSnapshot>) -> (Watcher, Sender<()>) {
    let (sender, receiver) = bounded(1);
    let watcher = Watcher {
        receiver,
        latest: Arc::new(Mutex::new(update)),
        stopping: ipc::SessionStop::default(),
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
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
        Arc::new(crate::i18n::Localizer::new("en")),
        &AtomicBool::new(false),
        watcher,
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
    use ipc::tests::{complete_test_handshake, read_test_query};
    use ratatui::{Terminal, backend::TestBackend};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
        Arc::new(crate::i18n::Localizer::new("en")),
        &AtomicBool::new(false),
        watcher,
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
    assert_eq!(pending.unwrap().read(&mut [0]).unwrap(), 0);
    server.join().unwrap();
}

#[test]
fn watcher_disconnection_exits_loading_and_existing_projection_as_an_error() {
    use ratatui::{Terminal, backend::TestBackend};

    for initialized in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let update = initialized.then(|| Ok((1, ClientSnapshot::default())));
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
            Arc::new(crate::i18n::Localizer::new("en")),
            &AtomicBool::new(false),
            watcher,
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
        assert!(
            error
                .to_string()
                .contains("disconnected without a shutdown snapshot")
        );
        assert_eq!(input_waits, usize::from(initialized));
    }
}

#[test]
fn watcher_reports_its_failure_instead_of_generic_disconnection() {
    let (watcher, sender) = injected_watcher(Some(Err("state read failed".into())));
    drop(sender);
    assert_eq!(
        watcher.take_update().unwrap_err().to_string(),
        "state read failed"
    );
}

#[test]
fn watcher_shutdown_closes_stalled_initial_handshake() {
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
fn replacing_queries_keeps_one_stalled_socket_until_worker_shutdown() {
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
            offset,
            ..request.clone()
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
fn replaced_queries_drain_then_send_only_latest_on_the_same_session() {
    use ipc::tests::{complete_test_handshake, read_test_query, reply_test_query};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
        for offset in [0, 99] {
            if offset != 0 {
                assert!(matches!(
                    read_test_query(&mut stream),
                    Query::QueuePage { offset: 99, .. }
                ));
            }
            reply_test_query(
                &mut stream,
                crate::response::QueryResponse {
                    revisions: QueryRevisions::default(),
                    result: Ok(ViewResponse::QueuePage(crate::model::Page {
                        total: 100,
                        rows: Vec::new(),
                    })),
                },
            );
        }
    });
    let mut worker = spawn_query_worker(directory.path(), 1);
    let request = QueryRequest {
        kind: QueryKind::Queue,
        offset: 0,
        limit: 20,
        generation: 0,
        stamp: Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        }
        .stamp(QueryRevisions::default()),
    };
    worker.request(request.clone());
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    for offset in 1..100 {
        worker.request(QueryRequest {
            offset,
            ..request.clone()
        });
    }
    assert!(worker.latest.lock().is_none());
    release.send(()).unwrap();
    // Finish the accepted jobs without shutdown cancellation so the final
    // consumer result can be inspected without polling or timing races.
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.offset, 99);
    assert!(matches!(result.unwrap().view, ViewResponse::QueuePage(page) if page.total == 100));
    server.join().unwrap();
}

#[test]
fn query_worker_shutdown_cancels_active_exchange_and_discards_pending_job() {
    use ipc::tests::{complete_test_handshake, read_test_query};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
    let request = QueryRequest {
        kind: QueryKind::Queue,
        offset: 0,
        limit: 20,
        generation: 0,
        stamp: Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        }
        .stamp(QueryRevisions::default()),
    };
    worker.request(request.clone());
    let mut stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.request(QueryRequest {
        offset: 99,
        ..request
    });
    let pending = Arc::clone(&worker.pending);
    let results = Arc::clone(&worker.latest);
    drop(worker);
    assert!(pending.lock().is_none());
    assert!(results.lock().is_none());
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    server.join().unwrap();
}

#[test]
fn command_worker_reports_ambiguous_failure_without_replaying_the_mutation() {
    use ipc::tests::{complete_test_handshake, read_test_command, reply_test_ack};
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
                status: crate::response::AckStatus::Succeeded,
                error: None,
                revision: 2,
                scan_id: None,
                relocation_id: None,
            },
        );
    });
    let worker = spawn_command_worker(directory.path(), 1);
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(Command::CreatePlaylist {
            name: "first".into(),
        })
        .unwrap();
    let failure = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(failure.message.contains("Command outcome unknown"));
    assert!(failure.message.contains("do not automatically retry"));
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap();
    let ack = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(ack.is_success());
    assert_eq!(ack.revision, 2);
    drop(worker);
    server.join().unwrap();
}

#[test]
fn command_worker_shutdown_closes_pending_exchange_and_drops_queued_commands() {
    use ipc::tests::{complete_test_handshake, read_test_command};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
        .send(Command::CreatePlaylist {
            name: "first".into(),
        })
        .unwrap();
    let mut stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker
        .sender
        .as_ref()
        .unwrap()
        .send(Command::CreatePlaylist {
            name: "second".into(),
        })
        .unwrap();
    drop(worker);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    server.join().unwrap();
}

#[test]
fn watcher_identity_binds_delayed_command_and_query_workers() {
    use ipc::tests::{complete_test_handshake, complete_test_handshake_for_instance};
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
        .send(Command::CreatePlaylist {
            name: "A-only".into(),
        })
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
    let request = QueryRequest {
        kind: QueryKind::Queue,
        offset: 0,
        limit: 20,
        generation: 0,
        stamp: Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        }
        .stamp(QueryRevisions::default()),
    };
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
    use ipc::tests::{
        complete_test_handshake, complete_test_handshake_for_instance, read_test_command,
    };
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
            .send(Command::CreatePlaylist { name: name.into() })
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
    use ipc::tests::complete_test_handshake_for_instance;
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
    worker.request(QueryRequest {
        kind: QueryKind::Queue,
        offset: 0,
        limit: 20,
        generation: 0,
        stamp: Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        }
        .stamp(QueryRevisions::default()),
    });
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
    use ipc::tests::{
        complete_test_handshake, complete_test_handshake_for_instance, read_test_query,
    };
    use std::{io::Read, os::unix::net::UnixListener};

    let directory = tempfile::tempdir().unwrap();
    let listener =
        UnixListener::bind_addr(&ipc::endpoint::address(directory.path()).unwrap()).unwrap();
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
    let request = QueryRequest {
        kind: QueryKind::Queue,
        offset: 0,
        limit: 20,
        generation: 0,
        stamp: Query::QueuePage {
            query: None,
            offset: 0,
            limit: 20,
        }
        .stamp(QueryRevisions::default()),
    };
    worker.request(request.clone());
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.request(QueryRequest {
        offset: 20,
        ..request
    });
    release.send(()).unwrap();
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.offset, 20);
    let failure = result.err().unwrap();
    assert!(failure.instance_changed);
    assert!(failure.into_message().is_err());
    server.join().unwrap();
}
