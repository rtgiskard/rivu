use rivu_protocol::{
    model::{Command, Query},
    response::{Ack, QueryResponse, StateResponse, StateRevisions},
    transport::{decode_request_frame, encode_response_frame_into},
    wire::{HelloResponse, MAX_REQUEST, WireRequest, WireResponse, wire_state},
};
/// Keeps the directory inode alive in forked children until exec or exit.
/// CLOEXEC listeners are inherited across fork too; without this pin, removing
/// a directory can recycle its IPC identity while a child still owns its socket.
pub struct SocketTestDirectory {
    directory: tempfile::TempDir,
    _identity: std::fs::File,
}

impl SocketTestDirectory {
    pub fn path(&self) -> &std::path::Path {
        self.directory.path()
    }
}

pub fn socket_test_directory() -> SocketTestDirectory {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    std::fs::create_dir_all(&cache).unwrap();
    let directory = tempfile::tempdir_in(cache).unwrap();
    let identity = std::fs::File::open(directory.path()).unwrap();
    SocketTestDirectory {
        directory,
        _identity: identity,
    }
}

pub fn read_test_request(stream: &mut std::os::unix::net::UnixStream) -> WireRequest {
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

pub fn complete_test_handshake(stream: &mut std::os::unix::net::UnixStream) {
    complete_test_handshake_for_instance(stream, 1);
}

pub fn complete_test_handshake_for_instance(
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
                processing: None,
            }),
        }),
    );
}

pub fn read_test_query(stream: &mut std::os::unix::net::UnixStream) -> Query {
    let query = match read_test_request(stream) {
        WireRequest::Query(query) => query,
        WireRequest::QueryRetained(query) => query.query,
        _ => panic!("expected query"),
    };
    Query::try_from(query).unwrap()
}

pub fn reply_test_query(stream: &mut std::os::unix::net::UnixStream, response: QueryResponse) {
    write_test_response(stream, WireResponse::Query(response));
}

pub fn read_test_command(stream: &mut std::os::unix::net::UnixStream) -> Command {
    let WireRequest::Command(command) = read_test_request(stream) else {
        panic!("expected command")
    };
    Command::try_from(command).unwrap()
}

pub fn reply_test_ack(stream: &mut std::os::unix::net::UnixStream, ack: Ack) {
    write_test_response(stream, WireResponse::Ack(ack));
}
