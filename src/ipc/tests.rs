use super::*;
use crate::app::AppHandle;
use crate::config::ConfigPersistence;
use rivu_client::client_session;
use rivu_protocol::{AppCommand, transport::*, wire::*};
use rivu_protocol::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions, StateSections},
};

#[test]
fn pending_query_preserves_pipelined_responses_and_disconnected_clients_release_slots() {
    use std::io::{Read, Write};
    use std::time::Duration;

    fn read_response(stream: &mut std::os::unix::net::UnixStream) -> WireResponse {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= MAX_RESPONSE);
        let mut frame = vec![0; length];
        stream.read_exact(&mut frame).unwrap();
        decode_response_frame(&frame).unwrap()
    }

    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let mut runtime = crate::core::CoreRuntime::start(directory.path(), &config_path).unwrap();
    let (handle, queries) = crate::core::CoreHandle::test_query_port();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(&owner, handle.clone(), app, runtime.test_executor()).unwrap();
    let address = rivu_protocol::wire::address(directory.path()).unwrap();
    let connect = || {
        let mut stream = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut frame = Vec::new();
        encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut frame).unwrap();
        stream.write_all(&frame).unwrap();
        assert!(matches!(read_response(&mut stream), WireResponse::Hello(_)));
        stream
    };
    let receive_query =
        || async_io::block_on(timeout(Duration::from_secs(2), queries.receive())).unwrap();
    let response = || QueryResponse {
        revisions: Default::default(),
        result: Ok(rivu_protocol::response::ViewResponse::Track(None)),
    };
    let mut stream = connect();
    let mut frame = Vec::new();
    encode_request_frame_into(
        &WireRequest::Query(WireQuery::from(Query::Track { track_id: 1 })),
        &mut frame,
    )
    .unwrap();
    stream.write_all(&frame).unwrap();
    let reply = receive_query();
    encode_request_frame_into(&WireRequest::State(StateSections::ALL), &mut frame).unwrap();
    stream.write_all(&frame).unwrap();
    encode_request_frame_into(
        &WireRequest::Query(WireQuery::from(Query::Track { track_id: 2 })),
        &mut frame,
    )
    .unwrap();
    stream.write_all(&frame[..1]).unwrap();
    stream.set_nonblocking(true).unwrap();
    assert_eq!(
        stream.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    stream.set_nonblocking(false).unwrap();
    assert!(!reply.is_cancelled());
    reply.respond(response());
    assert!(matches!(read_response(&mut stream), WireResponse::Query(_)));
    assert!(matches!(read_response(&mut stream), WireResponse::State(_)));
    stream.write_all(&frame[1..]).unwrap();
    receive_query().respond(response());
    assert!(matches!(read_response(&mut stream), WireResponse::Query(_)));
    drop(stream);

    // More than MAX_CLIENTS abandoned reads must still admit another Hello
    // without waiting for the controlled query consumer to send any response.
    for _ in 0..=MAX_CLIENTS {
        let mut stream = connect();
        encode_request_frame_into(
            &WireRequest::Query(WireQuery::from(Query::LibraryStats)),
            &mut frame,
        )
        .unwrap();
        stream.write_all(&frame).unwrap();
        let mut reply = receive_query();
        drop(stream);
        async_io::block_on(timeout(Duration::from_secs(2), reply.cancelled())).unwrap();
    }
    drop(connect());
    assert_eq!(handle.test_revision_subscriptions(), (0, 0));
    drop(server);
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
}

#[test]
fn configuration_only_change_wakes_ipc_watch_and_selective_read_omits_runtime() {
    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = crate::core::CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server =
        Server::start(&owner, runtime.handle.clone(), app, runtime.test_executor()).unwrap();
    let mut control = client_session(directory.path()).unwrap();
    control.query(Query::ScanTask { id: 0 }).unwrap();
    let initial = control.get_state(StateSections::ALL).unwrap();
    let before = initial.revisions;
    assert_eq!(initial.sections(), StateSections::ALL);
    assert_eq!(
        initial.configuration.as_ref().unwrap().config_path,
        config_path
    );
    let previous = initial.configuration.unwrap().config.waveform_seek_enabled;
    let mut watcher = client_session(directory.path()).unwrap();
    watcher
        .bind_instance(control.instance_id().unwrap())
        .unwrap();
    let stop = watcher.cancellation();
    let (completed, completion) = std::sync::mpsc::sync_channel(1);
    let watched = std::thread::spawn(move || {
        let changed = watcher.watch(before).unwrap().changed_since(before);
        assert_eq!(
            changed,
            StateSections {
                configuration: true,
                ..Default::default()
            }
        );
        completed.send(watcher.get_state(changed).unwrap()).unwrap();
    });
    runtime
        .handle
        .update_gui_config(crate::core::GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    // Bound the failure path without depending on scheduling a particular watch/read order.
    let response = completion.recv_timeout(std::time::Duration::from_secs(5));
    stop.cancel();
    watched.join().unwrap();
    let response = response.expect("configuration-only publication did not wake IPC Watch");
    assert_eq!(
        response.sections(),
        StateSections {
            configuration: true,
            ..Default::default()
        }
    );
    assert_ne!(
        response.configuration.unwrap().config.waveform_seek_enabled,
        previous
    );
    drop(server);
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
}

#[test]
fn app_commands_route_over_ipc_without_changing_business_state() {
    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = crate::core::CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server =
        Server::start(&owner, runtime.handle.clone(), app, runtime.test_executor()).unwrap();
    let mut client = client_session(directory.path()).unwrap();
    // A query fences asynchronous startup before comparing published state.
    client.query(Query::ScanTask { id: 0 }).unwrap();
    for target in [
        rivu_protocol::model::TaskTarget::Scan { id: u64::MAX },
        rivu_protocol::model::TaskTarget::Relocation { id: u64::MAX },
        rivu_protocol::model::TaskTarget::Processing { id: u64::MAX },
    ] {
        let instance = client.instance_id();
        let error = client.wait_task(target).unwrap_err();
        assert!(
            error
                .downcast_ref::<rivu_protocol::response::QueryError>()
                .is_some()
        );
        assert_eq!(client.instance_id(), instance);
    }
    let before = client.get_state(StateSections::ALL).unwrap();
    let rejected = client.request_ack(AppCommand::ShowWindow).unwrap();
    assert!(!rejected.is_success());
    assert!(
        rejected
            .error
            .as_deref()
            .unwrap()
            .contains("graphical host")
    );
    assert_eq!(rejected.revision, before.system.as_ref().unwrap().revision);
    let after = client.get_state(StateSections::ALL).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    let ack = client.request_ack(AppCommand::Shutdown).unwrap();
    assert!(ack.is_success());
    runtime.join().unwrap();
    assert!(runtime.handle.is_shutting_down());
    assert!(ack.revision <= runtime.handle.publication_revision());
    drop(server);
    assert!(SocketOwner::acquire(directory.path()).is_err());
    drop(owner);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn malformed_request_during_watch_receives_invalid_request_response() {
    use std::io::{Read, Write};

    fn read_response(stream: &mut std::os::unix::net::UnixStream) -> WireResponse {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).unwrap();
        let length = u32::from_le_bytes(prefix) as usize;
        assert!(length <= MAX_RESPONSE);
        let mut frame = vec![0; length];
        stream.read_exact(&mut frame).unwrap();
        decode_response_frame(&frame).unwrap()
    }

    let directory = socket_test_directory();
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = crate::core::CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server =
        Server::start(&owner, runtime.handle.clone(), app, runtime.test_executor()).unwrap();
    let mut control = client_session(directory.path()).unwrap();
    control.query(Query::ScanTask { id: 0 }).unwrap();
    let address = rivu_protocol::wire::address(directory.path()).unwrap();
    let mut stream = std::os::unix::net::UnixStream::connect_addr(&address).unwrap();
    let timeout = Some(std::time::Duration::from_secs(5));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    let mut encoded = Vec::new();
    encode_request_frame_into(&WireRequest::Hello(StateSections::ALL), &mut encoded).unwrap();
    stream.write_all(&encoded).unwrap();
    assert!(matches!(read_response(&mut stream), WireResponse::Hello(_)));

    encode_request_frame_into(
        &WireRequest::Watch(runtime.handle.state_revisions()),
        &mut encoded,
    )
    .unwrap();
    // A complete frame with an unknown request type is a decoding failure, not
    // a framing failure: Watch must hand it back to the normal error responder.
    encoded.extend_from_slice(&[2, 0, 0, 0, PROTOCOL_VERSION, 99]);
    stream.write_all(&encoded).unwrap();
    let response = match read_response(&mut stream) {
        // Startup publication may race the Watch baseline.
        WireResponse::Watch(_) => read_response(&mut stream),
        response => response,
    };
    assert!(matches!(
        response,
        WireResponse::Error(error) if error.starts_with("Invalid IPC request:")
    ));
    assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
    drop(stream);
    assert!(
        control
            .request_ack(AppCommand::Shutdown)
            .unwrap()
            .is_success()
    );
    runtime.join().unwrap();
    drop(server);
}

#[test]
fn library_root_queries_run_through_real_owner_and_ipc_session() {
    use crate::{config::Config, core::CoreRuntime, store::Store};
    use rivu_protocol::{
        model::{LibrarySort, MAX_QUERY_ROWS},
        response::{QueryError, ViewResponse},
    };
    let directory = socket_test_directory();
    let root = directory.path().join("音楽");
    let nested = root.join("夏");
    let empty = directory.path().join("empty");
    let database = directory.path().join("library.db");
    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    for (path, title, missing) in [
        (root.join("one.wav"), "Needle One", false),
        (nested.join("two.wav"), "Needle Two", true),
        (
            directory.path().join("音楽室/three.wav"),
            "Needle Outside",
            false,
        ),
    ] {
        connection
            .execute(
                "INSERT INTO tracks(path,raw_title,missing) VALUES(?1,?2,?3)",
                rusqlite::params![path.to_str().unwrap(), title, missing],
            )
            .unwrap();
    }
    drop(connection);
    let config_path = directory.path().join("config.toml");
    Config {
        library_roots: vec![root.clone(), empty.clone()],
        media_read_buffer_mb: 1,
        ..Config::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(
        &owner,
        runtime.handle.clone(),
        app.clone(),
        runtime.test_executor(),
    )
    .unwrap();
    assert!(SocketOwner::acquire(directory.path()).is_err());
    let mut session = client_session(directory.path()).unwrap();
    let count_query = Query::LibraryRootCounts {
        roots: vec![root.clone(), nested.clone(), empty],
    };
    let first = runtime.handle.query(count_query.clone());
    let remote = session.query(count_query.clone()).unwrap();
    assert_eq!(remote.revisions, first.revisions);
    let ViewResponse::LibraryRootCounts(counts) = remote.result.unwrap() else {
        panic!("expected root counts")
    };
    assert_eq!(
        counts.iter().map(|root| root.total).collect::<Vec<_>>(),
        [2, 1, 0]
    );
    let page_query = Query::LibraryRootPage {
        path: root,
        query: Some("needle".into()),
        favorite: None,
        missing: Some(false),
        sort: LibrarySort::Album,
        offset: 0,
        limit: 1,
    };
    let direct = runtime.handle.query(page_query.clone());
    let remote = session.query(page_query).unwrap();
    assert_eq!(
        serde_json::to_value(&remote).unwrap(),
        serde_json::to_value(&direct).unwrap()
    );
    let ViewResponse::LibraryPage(page) = remote.result.unwrap() else {
        panic!("expected library page")
    };
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].title, "Needle One");
    let ack = session
        .request_ack(Command::SetFavorite {
            track_ids: vec![page.rows[0].id],
            favorite: true,
        })
        .unwrap();
    assert!(ack.is_success());
    let updated = session.query(count_query).unwrap();
    assert_ne!(updated.revisions.library, first.revisions.library);
    assert_eq!(updated.revisions.structure, first.revisions.structure);
    let ViewResponse::LibraryRootCounts(counts) = updated.result.unwrap() else {
        panic!("expected root counts")
    };
    assert_eq!(
        counts.iter().map(|root| root.total).collect::<Vec<_>>(),
        [2, 1, 0]
    );
    assert!(matches!(
        session
            .query(Query::LibraryRootCounts {
                roots: vec![nested; MAX_QUERY_ROWS + 1]
            })
            .unwrap()
            .result,
        Err(QueryError::Message(_))
    ));
    drop(session);
    drop(server);
    // Stopping transport does not release the runtime's instance acquisition.
    assert!(SocketOwner::acquire(directory.path()).is_err());
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
    drop(owner);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn playlist_content_lifetimes_and_batches_are_isolated_from_the_catalog() {
    use crate::{config::Config, core::CoreRuntime, store::Store};
    use rivu_protocol::response::{PlaylistEntryRevision, QueryError, ViewResponse};
    let directory = socket_test_directory();
    let database = directory.path().join("library.db");
    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "INSERT INTO tracks(id,path,raw_title) VALUES (1,'one.wav','One'),(2,'two.wav','Two'),(3,'three.wav','Three');
         INSERT INTO playlists(id,name) VALUES (10,'one'),(20,'two');
         INSERT INTO playlist_entries(id,playlist_id,track_id,position)
         VALUES (101,10,1,0),(102,10,2,1),(201,20,1,0),(202,20,2,1);",
        )
        .unwrap();
    drop(connection);
    let config_path = directory.path().join("config.toml");
    Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Config::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(
        &owner,
        runtime.handle.clone(),
        app.clone(),
        runtime.test_executor(),
    )
    .unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let summaries = Query::PlaylistSummaries {
        query: None,
        offset: 0,
        limit: 2,
    };
    let catalog = session.query(summaries.clone()).unwrap();
    let batch = |playlist_id, expected| Query::PlaylistEntryBatch {
        query: None,
        playlist_id,
        after: None,
        limit: 1,
        expected,
    };
    let first = session.query(batch(10, Some(catalog.revisions))).unwrap();
    let other = session.query(batch(20, Some(catalog.revisions))).unwrap();
    let ViewResponse::PlaylistEntryBatch(first_page) = first.result.as_ref().unwrap() else {
        panic!("expected first playlist batch")
    };
    let first_cursor = first_page.next.unwrap();
    let ViewResponse::PlaylistEntryBatch(other_page) = other.result.as_ref().unwrap() else {
        panic!("expected other playlist batch")
    };
    let other_cursor = other_page.next.unwrap();
    assert_eq!(first_page.rows[0].id, 101);
    assert_eq!(other_page.rows[0].id, 201);
    let sections = StateSections {
        library: true,
        ..StateSections::default()
    };
    let before = session.get_state(sections).unwrap();
    let initial = before.library.as_ref().unwrap();
    assert_eq!(initial.playlist_entry_revisions.len(), 2);
    assert_eq!(
        initial.playlist_entry_revisions.get(&10),
        Some(&first.revisions.playlist_entries.unwrap().revision)
    );
    assert_eq!(
        initial.playlist_entry_revisions.get(&20),
        Some(&other.revisions.playlist_entries.unwrap().revision)
    );
    assert_ne!(
        first.revisions.playlist_entries.unwrap().revision,
        other.revisions.playlist_entries.unwrap().revision
    );
    // A catalog baseline can start a traversal, never continue one without E.
    assert_eq!(
        session
            .query(Query::PlaylistEntryBatch {
                query: None,
                playlist_id: 10,
                after: Some(first_cursor),
                limit: 1,
                expected: Some(catalog.revisions),
            })
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                before_entry_id: None,
                after: false,
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let after = session.get_state(sections).unwrap();
    assert_ne!(before.revisions.library, after.revisions.library);
    let library = after.library.unwrap();
    assert_eq!(library.playlist_revision, catalog.revisions.playlist);
    assert_eq!(library.revision, catalog.revisions.library);
    let moved_token = *library.playlist_entry_revisions.get(&10).unwrap();
    assert_ne!(
        moved_token,
        first.revisions.playlist_entries.unwrap().revision
    );
    assert_eq!(library.playlist_entry_revisions.len(), 2);
    assert_eq!(
        library.playlist_entry_revisions.get(&20),
        Some(&other.revisions.playlist_entries.unwrap().revision)
    );
    assert_eq!(
        library.duplicate_members_revision,
        initial.duplicate_members_revision
    );
    assert_eq!(
        serde_json::to_value(session.query(summaries).unwrap()).unwrap(),
        serde_json::to_value(&catalog).unwrap()
    );
    assert_eq!(
        session
            .query(batch(10, Some(first.revisions)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .query(batch(20, Some(other.revisions)))
            .unwrap()
            .result
            .is_ok()
    );
    let selected = session
        .query(Query::PlaylistEntry { entry_id: 101 })
        .unwrap();
    assert_eq!(
        selected.revisions.playlist_entries,
        Some(PlaylistEntryRevision {
            playlist_id: 10,
            revision: moved_token
        })
    );
    assert!(
        matches!(selected.result, Ok(ViewResponse::PlaylistEntry(Some(entry)))
        if entry.position == 1 && entry.id == 101)
    );
    let location_query = Query::PlaylistEntryLocations {
        query: None,
        playlist_id: 10,
        entry_ids: vec![101, 201, i64::MAX],
    };
    let locations = session.query(location_query.clone()).unwrap();
    assert_eq!(
        location_query.stamp(locations.revisions),
        location_query.stamp(selected.revisions)
    );
    assert!(
        matches!(locations.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.len() == 1 && rows[0].id == 101 && rows[0].position == 1)
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: first.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![101],
                expected_revision: moved_token,
            })
            .unwrap()
            .is_success()
    );
    let removed = session
        .query(Query::PlaylistEntryLocations {
            query: None,
            playlist_id: 10,
            entry_ids: vec![101],
        })
        .unwrap();
    assert!(
        matches!(removed.result, Ok(ViewResponse::PlaylistEntryLocations(rows)) if rows.is_empty())
    );
    assert_ne!(
        removed.revisions.playlist_entries,
        selected.revisions.playlist_entries
    );
    let expected = removed.revisions;
    assert!(
        session
            .request_ack(Command::RenamePlaylist {
                playlist_id: 10,
                name: "renamed".into()
            })
            .unwrap()
            .is_success()
    );
    let library = session.get_state(sections).unwrap().library.unwrap();
    assert_ne!(library.playlist_revision, expected.playlist);
    assert_eq!(
        library.duplicate_members_revision,
        expected.duplicate_members
    );
    assert_eq!(library.playlist_entry_revisions.len(), 2);
    assert_eq!(
        library.playlist_entry_revisions.get(&10),
        Some(&expected.playlist_entries.unwrap().revision)
    );
    assert!(
        session
            .query(batch(10, Some(expected)))
            .unwrap()
            .result
            .is_ok()
    );
    // Renames and membership changes elsewhere preserve a real continuation.
    assert!(
        session
            .request_ack(Command::RenamePlaylist {
                playlist_id: 20,
                name: "other renamed".into(),
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: 10,
                track_ids: vec![3],
            })
            .unwrap()
            .is_success()
    );
    let member_change = session.query(batch(10, None)).unwrap();
    assert_ne!(
        member_change.revisions.playlist_entries,
        expected.playlist_entries
    );
    assert_ne!(member_change.revisions.playlist, expected.playlist);
    assert_eq!(member_change.revisions.library, expected.library);
    assert!(member_change.result.is_ok());
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 10,
                entry_ids: vec![],
                expected_revision: expected.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let continuation = session
        .query(Query::PlaylistEntryBatch {
            query: None,
            playlist_id: 20,
            after: Some(other_cursor),
            limit: 1,
            expected: Some(other.revisions),
        })
        .unwrap();
    assert!(matches!(continuation.result,
        Ok(ViewResponse::PlaylistEntryBatch(page)) if page.rows.len() == 1
            && page.rows[0].id == 202 && page.next.is_none()));
    assert_eq!(
        continuation.revisions.playlist_entries,
        other.revisions.playlist_entries
    );
    assert_ne!(continuation.revisions.playlist, other.revisions.playlist);
    assert_ne!(
        continuation.revisions.duplicate_members,
        other.revisions.duplicate_members
    );
    assert_eq!(
        continuation.revisions.duplicate_groups,
        other.revisions.duplicate_groups
    );
    // P is irrelevant even when an unscoped baseline starts after catalog changes.
    assert!(
        session
            .query(batch(20, Some(catalog.revisions)))
            .unwrap()
            .result
            .is_ok()
    );
    let mut stale_library = catalog.revisions;
    stale_library.library = stale_library.library.wrapping_add(1);
    assert_eq!(
        session
            .query(batch(20, Some(stale_library)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    // The current scalar token stays usable across every catalog-only change.
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                before_entry_id: None,
                after: false,
                expected_revision: other.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let current = session.query(batch(20, None)).unwrap();
    assert_ne!(
        current.revisions.playlist_entries,
        other.revisions.playlist_entries
    );
    assert_eq!(
        session
            .query(Query::PlaylistEntryBatch {
                query: None,
                playlist_id: 20,
                after: Some(other_cursor),
                limit: 1,
                expected: Some(other.revisions),
            })
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                expected_revision: other.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let before_noop = session.get_state(sections).unwrap().library.unwrap();
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: 20,
                track_ids: vec![],
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::MovePlaylistEntries {
                playlist_id: 20,
                entry_ids: vec![201],
                before_entry_id: None,
                after: false,
                expected_revision: current.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    let after_noop = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(
        before_noop.playlist_entry_revisions,
        after_noop.playlist_entry_revisions
    );
    assert_eq!(before_noop.playlist_revision, after_noop.playlist_revision);
    assert_eq!(
        before_noop.duplicate_members_revision,
        after_noop.duplicate_members_revision
    );
    // Deletion removes the lifetime, rather than falling back to the catalog token.
    assert!(
        session
            .request_ack(Command::DeletePlaylist { playlist_id: 20 })
            .unwrap()
            .is_success()
    );
    let deleted = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(deleted.playlist_entry_revisions.len(), 1);
    assert!(!deleted.playlist_entry_revisions.contains_key(&20));
    assert_eq!(
        deleted.playlist_entry_revisions.get(&10),
        before_noop.playlist_entry_revisions.get(&10)
    );
    assert_eq!(
        session
            .query(batch(20, Some(current.revisions)))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let missing = session.query(batch(20, None)).unwrap();
    assert!(missing.result.is_err());
    assert!(missing.revisions.playlist_entries.is_none());
    for playlist_id in [20, i64::MAX] {
        assert!(
            !session
                .request_ack(Command::RemovePlaylistEntries {
                    playlist_id,
                    entry_ids: vec![],
                    expected_revision: current.revisions.playlist_entries.unwrap().revision,
                })
                .unwrap()
                .is_success()
        );
        assert!(
            !session
                .request_ack(Command::MovePlaylistEntries {
                    playlist_id,
                    entry_ids: vec![],
                    before_entry_id: None,
                    after: false,
                    expected_revision: current.revisions.playlist_entries.unwrap().revision,
                })
                .unwrap()
                .is_success()
        );
    }
    assert!(
        session
            .request_ack(Command::CreatePlaylist {
                name: "replacement".into()
            })
            .unwrap()
            .is_success()
    );
    let recreated = session.get_state(sections).unwrap().library.unwrap();
    assert_eq!(recreated.playlist_entry_revisions.len(), 2);
    assert_eq!(
        recreated.playlist_entry_revisions.get(&10),
        deleted.playlist_entry_revisions.get(&10)
    );
    let (&new_id, &new_revision) = recreated
        .playlist_entry_revisions
        .iter()
        .find(|(id, _)| **id != 10)
        .unwrap();
    assert_ne!(
        new_revision,
        current.revisions.playlist_entries.unwrap().revision
    );
    assert_ne!(
        new_revision,
        first.revisions.playlist_entries.unwrap().revision
    );
    assert!(
        !session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: new_id,
                entry_ids: vec![],
                expected_revision: current.revisions.playlist_entries.unwrap().revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntries {
                playlist_id: new_id,
                entry_ids: vec![],
                expected_revision: new_revision,
            })
            .unwrap()
            .is_success()
    );
    assert!(
        session
            .request_ack(Command::AddPlaylist {
                playlist_id: new_id,
                track_ids: vec![1],
            })
            .unwrap()
            .is_success()
    );
    let populated = session.query(batch(new_id, None)).unwrap();
    let ViewResponse::PlaylistEntryBatch(page) = populated.result.unwrap() else {
        panic!("expected populated playlist batch")
    };
    assert!(
        session
            .request_ack(Command::RemovePlaylistEntry {
                entry_id: page.rows[0].id,
            })
            .unwrap()
            .is_success()
    );
    let emptied = session.query(batch(new_id, None)).unwrap();
    assert_ne!(
        emptied.revisions.playlist_entries,
        populated.revisions.playlist_entries
    );
    assert!(
        matches!(emptied.result, Ok(ViewResponse::PlaylistEntryBatch(page)) if page.rows.is_empty())
    );
    drop(session);
    drop(server);
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
    drop(owner);
}

pub(crate) fn socket_test_directory() -> tempfile::TempDir {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    std::fs::create_dir_all(&cache).unwrap();
    tempfile::tempdir_in(cache).unwrap()
}

fn read_test_request(stream: &mut std::os::unix::net::UnixStream) -> WireRequest {
    use std::io::Read;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let length = u32::from_le_bytes(prefix) as usize;
    assert!(length <= MAX_REQUEST);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    decode_request_frame(&body).unwrap()
}

fn write_test_response(stream: &mut std::os::unix::net::UnixStream, response: WireResponse) {
    use std::io::Write;
    let mut frame = Vec::new();
    encode_response_frame_into(&response, &mut frame).unwrap();
    stream.write_all(&frame).unwrap();
}

pub(crate) fn complete_test_handshake(stream: &mut std::os::unix::net::UnixStream) {
    complete_test_handshake_for_instance(stream, 1);
}

pub(crate) fn complete_test_handshake_for_instance(
    stream: &mut std::os::unix::net::UnixStream,
    instance_id: u32,
) {
    let timeout = Some(std::time::Duration::from_secs(5));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    assert!(matches!(read_test_request(stream), WireRequest::Hello(_)));
    write_test_response(
        stream,
        WireResponse::Hello(HelloResponse {
            instance_id: instance_id.to_le_bytes(),
            state: wire_state(StateResponse {
                revisions: StateRevisions::default(),
                playback: None,
                queue: None,
                library: None,
                system: None,
                configuration: None,
            }),
        }),
    );
}

pub(crate) fn read_test_query(stream: &mut std::os::unix::net::UnixStream) -> Query {
    let WireRequest::Query(query) = read_test_request(stream) else {
        panic!("expected query")
    };
    Query::try_from(query).unwrap()
}

pub(crate) fn reply_test_query(
    stream: &mut std::os::unix::net::UnixStream,
    response: QueryResponse,
) {
    write_test_response(stream, WireResponse::Query(response));
}

pub(crate) fn read_test_command(stream: &mut std::os::unix::net::UnixStream) -> Command {
    let WireRequest::Command(command) = read_test_request(stream) else {
        panic!("expected command")
    };
    Command::try_from(command).unwrap()
}

pub(crate) fn reply_test_ack(stream: &mut std::os::unix::net::UnixStream, ack: Ack) {
    write_test_response(stream, WireResponse::Ack(ack));
}

#[test]
fn instance_acquisition_creates_directory_without_socket_or_lock_files() {
    let directory = socket_test_directory();
    let data_dir = directory.path().join("data");
    let owner = SocketOwner::acquire(&data_dir).unwrap();
    assert!(data_dir.is_dir());
    assert_eq!(std::fs::read_dir(&data_dir).unwrap().count(), 0);
    assert!(SocketOwner::acquire(&data_dir).is_err());
    let address = rivu_protocol::wire::address(&data_dir).unwrap();
    assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    drop(owner);
    assert_eq!(std::fs::read_dir(&data_dir).unwrap().count(), 0);
    assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_err());
    assert!(SocketOwner::acquire(&data_dir).is_ok());
}

#[test]
fn directory_aliases_share_instance_ownership_and_transport() {
    let directory = socket_test_directory();
    let data_dir = directory.path().join("data");
    let owner = SocketOwner::acquire(&data_dir).unwrap();
    let symlink = directory.path().join("alias");
    std::os::unix::fs::symlink(&data_dir, &symlink).unwrap();
    let current_dir = std::env::current_dir().unwrap();
    let relative = data_dir.strip_prefix(&current_dir).unwrap();
    for alias in [symlink.as_path(), relative] {
        assert!(SocketOwner::acquire(alias).is_err());
        let address = rivu_protocol::wire::address(alias).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    }
    drop(owner);
    let owner = SocketOwner::acquire(&symlink).unwrap();
    assert!(SocketOwner::acquire(&data_dir).is_err());
    drop(owner);
    assert!(SocketOwner::acquire(relative).is_ok());
}

#[test]
fn distinct_data_directories_acquire_independent_instances() {
    let directory = socket_test_directory();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let first_owner = SocketOwner::acquire(&first).unwrap();
    let second_owner = SocketOwner::acquire(&second).unwrap();
    for data_dir in [&first, &second] {
        assert!(SocketOwner::acquire(data_dir).is_err());
        let address = rivu_protocol::wire::address(data_dir).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    }
    drop(first_owner);
    let first_owner = SocketOwner::acquire(&first).unwrap();
    assert!(SocketOwner::acquire(&second).is_err());
    drop(second_owner);
    assert!(SocketOwner::acquire(&second).is_ok());
    drop(first_owner);
}

#[test]
fn simultaneous_starters_acquire_one_real_listener() {
    let directory = socket_test_directory();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    SocketOwner::acquire(directory.path())
                })
            })
            .collect();
        let owners: Vec<_> = handles
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().ok())
            .collect();
        assert_eq!(owners.len(), 1);
        let address = rivu_protocol::wire::address(directory.path()).unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_ok());
    });
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    assert!(SocketOwner::acquire(directory.path()).is_ok());
}

#[test]
fn filtered_occurrence_continuations_round_trip_and_reject_stale_ipc_scopes() {
    use rivu_protocol::response::{QueryError, ViewResponse};
    let directory = socket_test_directory();
    let database = directory.path().join("library.db");
    drop(crate::store::Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.execute_batch(
        "INSERT INTO tracks(id,path,raw_title) VALUES
             (1,'one.wav','other'),(2,'two.wav','needle'),(3,'three.wav','other'),
             (4,'four.wav','other'),(5,'five.wav','needle'),(6,'six.wav','other'),(7,'seven.wav','needle');
         INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES (0,1,0),(0,2,1),(0,2,2),(0,3,3),(0,4,4),(0,5,5),(0,6,6),(0,7,7);
         INSERT INTO playlists(id,name) VALUES(10,'first'),(20,'second');
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 10,id,id-1 FROM tracks;
         INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(10,2,7);
         INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT 20,id,id-1 FROM tracks;",
    ).unwrap();
    drop(connection);
    let config_path = directory.path().join("config.toml");
    crate::config::Config {
        media_read_buffer_mb: 1,
        ffmpeg_enabled: false,
        ..Default::default()
    }
    .save(&config_path)
    .unwrap();
    let owner = SocketOwner::acquire(directory.path()).unwrap();
    let mut runtime = crate::core::CoreRuntime::start(directory.path(), &config_path).unwrap();
    let app = AppHandle::new(&runtime, false);
    let server = Server::start(
        &owner,
        runtime.handle.clone(),
        app.clone(),
        runtime.test_executor(),
    )
    .unwrap();
    let mut session = client_session(directory.path()).unwrap();
    let queue_query = |query: &str, after| Query::QueueBatch {
        query: Some(query.into()),
        after,
        limit: 1,
        expected: None,
    };
    let entry_query = |playlist_id, query: &str, after| Query::PlaylistEntryBatch {
        playlist_id,
        query: Some(query.into()),
        after,
        limit: 1,
        expected: None,
    };
    let mut after = None;
    let mut first_queue = None;
    let mut positions = Vec::new();
    loop {
        let ViewResponse::QueueBatch(batch) = session
            .query(queue_query("needle", after))
            .unwrap()
            .result
            .unwrap()
        else {
            panic!("expected queue batch")
        };
        assert_eq!(batch.rows[0].offset, positions.len());
        positions.push(batch.rows[0].position);
        after = batch.next;
        if first_queue.is_none() {
            first_queue = after;
        }
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, positions.len());
    }
    assert_eq!(positions, [1, 2, 5, 7]);
    assert_eq!(
        session
            .query(queue_query("other", first_queue))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let mut after = None;
    let mut first_entry = None;
    let mut positions = Vec::new();
    loop {
        let ViewResponse::PlaylistEntryBatch(batch) = session
            .query(entry_query(10, "needle", after))
            .unwrap()
            .result
            .unwrap()
        else {
            panic!("expected playlist batch")
        };
        assert_eq!(batch.rows[0].offset, positions.len());
        positions.push(batch.rows[0].position);
        after = batch.next;
        if first_entry.is_none() {
            first_entry = after;
        }
        if after.is_none() {
            break;
        }
        assert_eq!(after.unwrap().offset, positions.len());
    }
    assert_eq!(positions, [1, 4, 6, 7]);
    assert_eq!(
        session
            .query(entry_query(20, "needle", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(
        session
            .query(entry_query(10, "other", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert!(
        session
            .request_ack(Command::EditTrack {
                track_id: 2,
                title: Some("other".into()),
                artist: None,
                album: None,
                decoder_hint: None,
            })
            .unwrap()
            .is_success()
    );
    assert_eq!(
        session
            .query(queue_query("needle", first_queue))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    assert_eq!(
        session
            .query(entry_query(10, "needle", first_entry))
            .unwrap()
            .result
            .unwrap_err(),
        QueryError::RevisionChanged
    );
    let ViewResponse::PlaylistEntryBatch(restarted) = session
        .query(entry_query(10, "needle", None))
        .unwrap()
        .result
        .unwrap()
    else {
        panic!("expected restarted playlist batch")
    };
    assert_eq!(
        (restarted.rows[0].position, restarted.rows[0].offset),
        (4, 0)
    );
    drop(session);
    drop(server);
    app.request_shutdown().unwrap();
    runtime.join().unwrap();
}
