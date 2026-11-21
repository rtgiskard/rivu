use super::*;
use rivu::test_support::ipc::{SocketTestDirectory, socket_test_directory};
use rivu_protocol::model::Command;
use rivu_protocol::query_tracking::QueryTracker;
use rivu_protocol::response::QueryRevisions;
use rivu_protocol::test_support as ipc;
use std::time::Duration;

#[test]
fn read_kind_changes_project_terminal_revisions_for_their_query() {
    let previous = TuiSnapshot::default();
    let mut bitrate = previous.clone();
    bitrate.library.bitrate_revision = 1;
    let queue = ReadKind::Queue.changes(&previous, &bitrate, "");
    assert!(queue.rows_changed && !queue.coordinates_changed);
    let count = ReadKind::OtherTracksCount.changes(&previous, &bitrate, "");
    assert!(!count.rows_changed && !count.coordinates_changed);

    let mut metadata = previous.clone();
    metadata.library.revision += 1;
    let lookup = ReadKind::PlaylistSelection {
        playlist_id: 7,
        entry_id: 42,
    };
    assert!(!lookup.changes(&previous, &metadata, "").coordinates_changed);
    assert!(
        lookup
            .changes(&previous, &metadata, "needle")
            .coordinates_changed
    );
}

fn assert_query_socket_closed(mut stream: std::os::unix::net::UnixStream) {
    use std::io::Read;
    let mut received = Vec::new();
    stream.read_to_end(&mut received).unwrap();
    let mut pending = received.as_slice();
    while !pending.is_empty() {
        let length = u32::from_le_bytes(pending[..4].try_into().unwrap()) as usize;
        assert!(matches!(
            rivu_protocol::transport::decode_request_frame(&pending[4..4 + length]).unwrap(),
            rivu_protocol::wire::WireRequest::CancelRead(_)
        ));
        pending = &pending[4 + length..];
    }
}

/// A fake Core endpoint whose directory identity outlives forked test children.
fn fake_core() -> (SocketTestDirectory, std::os::unix::net::UnixListener) {
    let directory = socket_test_directory();
    let listener = std::os::unix::net::UnixListener::bind_addr(
        &rivu_protocol::wire::address(directory.path()).unwrap(),
    )
    .unwrap();
    (directory, listener)
}

fn created_playlist(name: &str) -> AppCommand {
    Command::CreatePlaylist { name: name.into() }.into()
}

fn queue_request(
    tracker: &mut QueryTracker,
    offset: usize,
    revisions: QueryRevisions,
) -> QueryRequest {
    let mut state = TuiSnapshot::default();
    state.queue.revision = revisions.queue;
    state.library.bitrate_revision = revisions.bitrate;
    let demand = ReadDemand {
        kind: ReadKind::Queue,
        filter: None,
        offset,
        limit: 20,
    }
    .own(&state);
    tracker.cancel();
    let submission = tracker.submit(demand.protocol_query(), revisions);
    QueryRequest {
        demand: Arc::new(demand),
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

    let (directory, listener) = fake_core();
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

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
        let directory = socket_test_directory();
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
fn command_and_query_worker_death_exit_the_owner_before_input() {
    use ratatui::{Terminal, backend::TestBackend};

    for command_dies in [true, false] {
        for panics in [false, true] {
            let directory = socket_test_directory();
            let commands = if command_dies {
                let (sender, receiver) = bounded(1);
                let (result_sender, results) = bounded(1);
                let worker = std::thread::spawn(move || {
                    let _requests = receiver;
                    let _results = result_sender;
                    assert!(!panics, "injected command worker panic");
                });
                assert!(matches!(
                    results.recv_timeout(Duration::from_secs(5)),
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected)
                ));
                let commands = CommandWorker {
                    sender: Some(sender),
                    stopping: None,
                    shutdown: rivu_client::SessionCancellation::default(),
                    results,
                    worker: Some(worker),
                };
                assert!(commands.take_result().is_err());
                assert!(
                    commands
                        .request(AppCommand::Shutdown, CommandFeedback::Notice)
                        .is_err()
                );
                commands
            } else {
                spawn_command_worker(directory.path(), 1)
            };
            let queries = if command_dies {
                spawn_query_worker(directory.path(), 1)
            } else {
                let (sender, receiver) = bounded(1);
                let (alive, liveness) = bounded(0);
                let worker = std::thread::spawn(move || {
                    let _requests = receiver;
                    let _alive = alive;
                    assert!(!panics, "injected query worker panic");
                });
                assert!(matches!(
                    liveness.recv_timeout(Duration::from_secs(5)),
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected)
                ));
                let mut tracker = QueryTracker::default();
                let request = queue_request(&mut tracker, 0, QueryRevisions::default());
                QueryWorker {
                    sender: Some(sender),
                    pending: Arc::new(Mutex::new(Some(QueryJob {
                        request,
                        ticket: tracker.ticket(),
                    }))),
                    latest: Arc::new(Mutex::new(None)),
                    liveness,
                    shutdown: rivu_client::SessionCancellation::default(),
                    worker: Some(worker),
                }
            };
            let command_shutdown = commands.shutdown.clone();
            let query_shutdown = queries.shutdown.clone();
            let pending = Arc::clone(&queries.pending);
            let mut workers = Some((commands, queries));
            let mut initial = Some((1, ClientSnapshot::default()));
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let error = crate::terminal::run_loop_with_workers(
                directory.path(),
                &mut terminal,
                Arc::new(rivu_presentation::i18n::Localizer::for_terminal("en")),
                &AtomicBool::new(false),
                || Ok(initial.take()),
                |_| panic!("dead workers must not leave the owner polling for input"),
                |_, _| workers.take().unwrap(),
            )
            .unwrap_err();
            assert!(!error.is::<QueryError>());
            assert!(command_shutdown.is_cancelled());
            assert!(query_shutdown.is_cancelled());
            assert!(pending.lock().is_none());
        }
    }
}

#[test]
fn command_worker_empty_results_and_full_queue_remain_live() {
    use rivu_protocol::wire::WireRequest;
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let worker = spawn_command_worker(directory.path(), 1);
    assert!(worker.take_result().unwrap().is_none());
    assert!(
        worker
            .request(AppCommand::Shutdown, CommandFeedback::Notice)
            .unwrap()
    );
    let mut stream = listener.accept().unwrap().0;
    assert!(matches!(
        ipc::read_test_request(&mut stream),
        WireRequest::Hello(_)
    ));
    // Hello is physically pending, so all eight queue slots can be filled
    // without racing command completion or mistaking the worker for dead.
    for _ in 0..8 {
        assert!(
            worker
                .request(AppCommand::Shutdown, CommandFeedback::Notice)
                .unwrap()
        );
    }
    assert!(
        !worker
            .request(AppCommand::Shutdown, CommandFeedback::Notice)
            .unwrap()
    );
    assert!(worker.take_result().unwrap().is_none());
    drop(worker);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

#[test]
fn watcher_shutdown_closes_stalled_initial_handshake() {
    use rivu_protocol::wire::WireRequest;
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let watcher = spawn_watcher(directory.path(), 1);
    let mut stream = listener.accept().unwrap().0;
    assert!(matches!(
        ipc::read_test_request(&mut stream),
        WireRequest::Hello(_)
    ));
    // Hello has reached the peer, which deliberately never replies. The
    // loading screen's exit must join the watcher without its response timeout.
    drop(watcher);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

#[test]
fn watcher_reports_initial_connection_failure_to_loading_consumer() {
    let directory = socket_test_directory();
    let watcher = spawn_watcher(directory.path(), 1);
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(watcher.take_update().is_err());
}

#[test]
fn watcher_rejects_replacement_at_initial_hello() {
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let watcher = spawn_watcher(directory.path(), 1);
    let mut stream = listener.accept().unwrap().0;
    ipc::complete_test_handshake_for_instance(&mut stream, 2);
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let error = watcher.take_update().expect_err("replacement rejected");
    assert!(error.to_string().contains("instance changed"));
    assert_eq!(
        stream.read(&mut [0]).unwrap(),
        0,
        "replacement received a watch"
    );
}

#[test]
fn watcher_applies_wait_deltas_without_another_read_and_keeps_shutdown() {
    use rivu_protocol::{
        response::{StateResponse, StateRevisions},
        test_support::write_test_response,
        wire::{HelloResponse, WireRequest, WireResponse, wire_state},
    };

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let (continue_watch, continuation) = bounded(1);
    let (ignored, ignored_update) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::Hello(sections)
            if sections == TERMINAL_SECTIONS)
        );
        write_test_response(
            &mut stream,
            WireResponse::Hello(HelloResponse {
                instance_id: 1u32.to_le_bytes(),
                state: wire_state(StateResponse::default()),
            }),
        );
        assert!(
            matches!(ipc::read_test_request(&mut stream), WireRequest::WaitState(watch)
            if watch.sections == TERMINAL_SECTIONS)
        );
        continuation.recv_timeout(Duration::from_secs(5)).unwrap();
        let processing_only = StateRevisions {
            processing: 1,
            ..Default::default()
        };
        // No interested payload means no publication or applied token advance.
        write_test_response(
            &mut stream,
            WireResponse::State(wire_state(StateResponse {
                revisions: processing_only,
                ..Default::default()
            })),
        );
        assert!(matches!(
            ipc::read_test_request(&mut stream),
            WireRequest::WaitState(watch) if watch.revisions == StateRevisions::default()
                && watch.sections == TERMINAL_SECTIONS
        ));
        ignored.send(()).unwrap();
        continuation.recv_timeout(Duration::from_secs(5)).unwrap();
        write_test_response(
            &mut stream,
            WireResponse::State(wire_state(StateResponse {
                revisions: StateRevisions {
                    playback: 2,
                    system: 1,
                    ..processing_only
                },
                playback: Some(rivu_protocol::response::PlaybackSnapshot {
                    playback: rivu_protocol::model::PlaybackState {
                        volume: 0.42,
                        ..Default::default()
                    },
                    processing_audition: None,
                    processing_selection: None,
                }),
                system: Some(rivu_protocol::model::SystemState {
                    shutting_down: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        );
    });
    let watcher = spawn_watcher(directory.path(), 1);
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
    assert_eq!(shutdown.playback.volume, 0.42);
    server.join().unwrap();
}

#[test]
fn replacing_queries_keeps_one_stalled_socket_until_worker_shutdown() {
    use rivu_protocol::wire::WireRequest;
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let mut worker = spawn_query_worker(directory.path(), 1);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    worker.request(request, tracker.ticket()).unwrap();
    let mut stream = listener.accept().unwrap().0;
    // The peer deliberately stalls Hello. Replacements must not abandon
    // the active connection and start more server work behind it.
    assert!(matches!(
        ipc::read_test_request(&mut stream),
        WireRequest::Hello(_)
    ));
    for offset in 1..100 {
        worker
            .request(
                queue_request(&mut tracker, offset, QueryRevisions::default()),
                tracker.ticket(),
            )
            .unwrap();
    }
    assert!(worker.take_result().unwrap().is_none());
    // Shutdown joins the worker, so any connection it ever opened is queued now.
    drop(worker);
    assert_eq!(
        stream.read(&mut [0]).unwrap(),
        0,
        "worker shutdown must close a stalled connection"
    );
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "replacement abandoned an active server connection"
    );
}

#[test]
fn replaced_bitrate_queries_drain_then_send_only_latest_on_the_same_session() {
    use ipc::{complete_test_handshake, read_test_query, read_test_request, reply_test_query};
    use rivu_protocol::wire::{WireQuery, WireRequest};

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let (started, active) = bounded(1);
    let (release, resume) = bounded(1);
    let (cancel_seen, cancellation) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        let WireRequest::Query(query) = read_test_request(&mut stream) else {
            panic!("expected retained query");
        };
        assert!(matches!(
            query.query,
            WireQuery::QueuePage { offset: 0, .. }
        ));
        started.send(()).unwrap();
        assert!(
            matches!(read_test_request(&mut stream), WireRequest::CancelRead(id) if id == query.id)
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
    worker.request(request.clone(), tracker.ticket()).unwrap();
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    for offset in 1..100 {
        worker
            .request(
                queue_request(
                    &mut tracker,
                    offset,
                    QueryRevisions::default().with_bitrate_revision(offset as u64),
                ),
                tracker.ticket(),
            )
            .unwrap();
    }
    cancellation.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(worker.take_result().unwrap().is_none());
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

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
    worker.request(request.clone(), tracker.ticket()).unwrap();
    let stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker
        .request(
            queue_request(&mut tracker, 99, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
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
    use ipc::{
        complete_test_handshake, complete_test_handshake_for_instance, read_test_command,
        reply_test_command,
    };
    use rivu_protocol::response::{Ack, AckStatus};

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    for replacement in [false, true] {
        let (directory, listener) = fake_core();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "first")
            );
            // Mutation execution is ambiguous to the client: no receipt arrives.
            drop(stream);
            let mut stream = listener.accept().unwrap().0;
            if replacement {
                use std::io::Read;
                complete_test_handshake_for_instance(&mut stream, 2);
                assert_eq!(stream.read(&mut [0]).unwrap(), 0, "no mutation on B");
                return;
            }
            complete_test_handshake(&mut stream);
            assert!(
                matches!(read_test_command(&mut stream), Command::CreatePlaylist { name } if name == "second")
            );
            reply_test_command(&mut stream, Ack::new(AckStatus::Succeeded, None, 2).into());
        });
        let worker = spawn_command_worker(directory.path(), 1);
        assert!(
            worker
                .request(created_playlist("first"), CommandFeedback::Notice)
                .unwrap()
        );
        let failure = worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .result
            .unwrap_err();
        failure.into_message().unwrap();
        assert!(
            worker
                .request(created_playlist("second"), CommandFeedback::Notice)
                .unwrap()
        );
        let second = worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .result;
        if replacement {
            assert!(second.unwrap_err().into_message().is_err());
        } else {
            assert!(second.unwrap().is_success());
        }
        drop(worker);
        server.join().unwrap();
    }
}

#[test]
fn command_worker_shutdown_closes_pending_exchange_and_drops_queued_commands() {
    use ipc::{complete_test_handshake, read_test_command};
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
    assert!(
        worker
            .request(created_playlist("first"), CommandFeedback::Notice)
            .unwrap()
    );
    let mut stream = active.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        worker
            .request(created_playlist("second"), CommandFeedback::Notice)
            .unwrap()
    );
    drop(worker);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    server.join().unwrap();
}

#[test]
fn watcher_identity_binds_delayed_command_and_query_workers() {
    use ipc::{complete_test_handshake, complete_test_handshake_for_instance};
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
    let watcher = spawn_watcher(directory.path(), 1);
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (instance_id, _) = watcher.latest.lock().take().unwrap().unwrap();
    assert_eq!(instance_id, 1);
    let mut commands = spawn_command_worker(directory.path(), instance_id);
    assert!(
        commands
            .request(created_playlist("A-only"), CommandFeedback::Notice)
            .unwrap()
    );
    commands.worker.take().unwrap().join().unwrap();
    // A disconnected submission must preserve the buffered identity failure,
    // rather than reporting either queue saturation or generic worker death.
    let error = commands
        .request(AppCommand::Shutdown, CommandFeedback::Notice)
        .unwrap_err();
    assert!(error.is::<QueryError>());
    let mut queries = spawn_query_worker(directory.path(), instance_id);
    let mut tracker = QueryTracker::default();
    let request = queue_request(&mut tracker, 0, QueryRevisions::default());
    queries.request(request, tracker.ticket()).unwrap();
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
fn superseded_query_still_reports_replacement_instance_as_fatal() {
    use ipc::complete_test_handshake_for_instance;
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
    worker
        .request(
            queue_request(&mut tracker, 0, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
    connected.recv_timeout(Duration::from_secs(5)).unwrap();
    tracker.cancel();
    release.send(()).unwrap();
    worker.worker.take().unwrap().join().unwrap();
    let error = worker
        .request(
            queue_request(&mut tracker, 1, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap_err();
    assert!(error.is::<QueryError>());
    // Even after its publisher has exited and demand was withdrawn, identity
    // invalidation wins over the liveness failure and reaches the owner.
    let (_, result) = worker.take_result().unwrap().unwrap();
    let failure = result.err().unwrap();
    assert!(failure.instance_changed);
    assert!(failure.into_message().is_err());
    server.join().unwrap();
}

#[test]
fn query_worker_reconnection_rejects_replacement_before_accepting_results() {
    use ipc::{complete_test_handshake, complete_test_handshake_for_instance, read_test_query};
    use std::io::Read;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
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
    worker.request(request.clone(), tracker.ticket()).unwrap();
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker
        .request(
            queue_request(&mut tracker, 20, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
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
    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(30)) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let _core =
        rivu::test_support::frontend::FrontendFixture::start(directory.path(), &config).unwrap();
    let mut bootstrap = rivu_client::client_session(directory.path());
    bootstrap.get_state(Default::default()).unwrap();
    let watcher = spawn_watcher(directory.path(), bootstrap.instance_id().unwrap());
    watcher
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (instance, initial) = watcher.take_update().unwrap().unwrap();
    assert!(!initial.system.shutting_down);
    let worker = spawn_command_worker(directory.path(), instance);
    assert!(
        worker
            .request(AppCommand::Shutdown, CommandFeedback::Notice)
            .unwrap()
    );
    assert!(
        worker
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .result
            .unwrap()
            .is_success()
    );
    // Only the watch's real shutdown state authorizes the TUI to exit; no
    // command-side revision refresh is required after the successful receipt.
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
    use ipc::{complete_test_handshake, read_test_request, reply_test_command};
    use rivu_protocol::{
        response::AckStatus,
        wire::{WireCommand, WireRequest},
    };

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        for rejected in [true, false] {
            assert!(matches!(
                read_test_request(&mut stream),
                WireRequest::Command(WireCommand::Shutdown)
            ));
            if rejected {
                reply_test_command(
                    &mut stream,
                    rivu_protocol::response::Ack::new(
                        AckStatus::NotAdmitted,
                        Some("shutdown rejected".into()),
                        0,
                    )
                    .into(),
                );
            }
        }
        drop(stream);
        listener
    });
    let worker = spawn_command_worker(directory.path(), 1);
    assert!(
        worker
            .request(AppCommand::Shutdown, CommandFeedback::Notice)
            .unwrap()
    );
    let ack = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .result
        .unwrap();
    assert!(!ack.is_success());
    assert!(
        worker
            .request(AppCommand::Shutdown, CommandFeedback::Notice)
            .unwrap()
    );
    let error = worker
        .results
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .result
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
        ReadKind::LibrarySelection(11),
        ReadKind::Directory(Path::new("/音乐/夜航")),
        ReadKind::DirectorySelection {
            track_id: 11,
            path: Path::new("/音乐/夜航"),
        },
        ReadKind::OtherTracks {
            favorite: Some(true),
            missing: None,
            sort: LibrarySort::Album,
            anchor: None,
        },
        ReadKind::OtherTracks {
            favorite: None,
            missing: Some(false),
            sort: LibrarySort::MostPlayed,
            anchor: Some(11),
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
        assert_eq!(
            absent.matches(&absent_spec, &next),
            matches!(
                kind,
                ReadKind::Queue | ReadKind::QueueSelection(_) | ReadKind::PlaylistSelection { .. }
            )
        );
        assert_eq!(
            empty.matches(&empty_spec, &next),
            matches!(kind, ReadKind::Queue | ReadKind::QueueSelection(_))
        );
    }
}

#[test]
fn repeated_withdrawal_discards_pending_but_retains_active_until_reply() {
    use ipc::{complete_test_handshake, read_test_query, read_test_request, reply_test_query};
    use rivu_protocol::wire::WireRequest;

    if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
        return;
    }
    let (directory, listener) = fake_core();
    let (started, active) = bounded(1);
    let (cancel_seen, cancelled) = bounded(1);
    let (release, resume) = bounded(1);
    let server = std::thread::spawn(move || {
        let mut stream = listener.accept().unwrap().0;
        complete_test_handshake(&mut stream);
        let WireRequest::Query(query) = read_test_request(&mut stream) else {
            panic!("expected retained query");
        };
        started.send(()).unwrap();
        assert!(matches!(
            read_test_request(&mut stream),
            WireRequest::CancelRead(id) if id == query.id
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
    // Retiring idle demand must leave its next real submission usable.
    tracker.cancel();
    tracker.cancel();
    worker
        .request(
            queue_request(&mut tracker, 0, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
    active.recv_timeout(Duration::from_secs(5)).unwrap();
    worker
        .request(
            queue_request(&mut tracker, 1, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
    tracker.cancel();
    tracker.cancel();
    cancelled.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(worker.take_result().unwrap().is_none());
    worker
        .request(
            queue_request(&mut tracker, 2, QueryRevisions::default()),
            tracker.ticket(),
        )
        .unwrap();
    release.send(()).unwrap();
    worker.sender.take();
    worker.worker.take().unwrap().join().unwrap();
    let (request, result) = worker.latest.lock().take().unwrap();
    assert_eq!(request.demand.offset, 2);
    assert!(matches!(result.unwrap().view, ViewResponse::QueuePage(page) if page.total == 100));
    server.join().unwrap();
}
