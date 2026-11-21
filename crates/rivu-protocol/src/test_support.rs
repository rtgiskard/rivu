//! Fixtures for tests of protocol consumers.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::model::{Command, Query, Track};
use crate::response::{Ack, QueryResponse, StateResponse, StateRevisions};
use crate::transport::{decode_request_frame, encode_response_frame_into};
use crate::wire::{HelloResponse, MAX_REQUEST, WireRequest, WireResponse, wire_state};

/// A synthetic stereo track, without reading the filesystem.
pub fn track(id: i64, path: impl Into<PathBuf>) -> Track {
    Track {
        id,
        path: path.into(),
        fingerprint: None,
        cue: None,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: None,
        codec: String::new(),
        channels: 2,
        sample_rate: 44_100,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: None,
        release_date: None,
        replaygain_track_gain: None,
        favorite: false,
        decoder_hint: Default::default(),
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

/// Track fixture for the mono, 8 kHz WAVs used in artwork tests.
pub fn artwork_track(path: PathBuf) -> Track {
    let fingerprint = path
        .is_file()
        .then(|| std::fs::read(&path).unwrap())
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string());
    Track {
        fingerprint,
        codec: "wav".into(),
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: Some(16),
        ..track(1, path)
    }
}

pub fn read_test_request(stream: &mut UnixStream) -> WireRequest {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let length = u32::from_le_bytes(prefix) as usize;
    assert!(length <= MAX_REQUEST);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    decode_request_frame(&body).unwrap()
}

pub fn write_test_response(stream: &mut UnixStream, response: WireResponse) {
    let mut frame = Vec::new();
    encode_response_frame_into(&response, &mut frame).unwrap();
    stream.write_all(&frame).unwrap();
}

pub fn complete_test_handshake(stream: &mut UnixStream) {
    complete_test_handshake_for_instance(stream, 1);
}

pub fn complete_test_handshake_for_instance(stream: &mut UnixStream, instance_id: u32) {
    let timeout = Some(Duration::from_secs(5));
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

pub fn read_test_command(stream: &mut UnixStream) -> Command {
    let WireRequest::Command(command) = read_test_request(stream) else {
        panic!("expected command");
    };
    Command::try_from(command).unwrap()
}

pub fn reply_test_ack(stream: &mut UnixStream, ack: Ack) {
    write_test_response(stream, WireResponse::Ack(ack));
}

/// Reads either query lane. Tests that require one lane should match its wire request.
pub fn read_test_query(stream: &mut UnixStream) -> Query {
    let query = match read_test_request(stream) {
        WireRequest::Query(query) => query,
        WireRequest::QueryRetained(query) => query.query,
        _ => panic!("expected query"),
    };
    Query::try_from(query).unwrap()
}

pub fn reply_test_query(stream: &mut UnixStream, response: QueryResponse) {
    write_test_response(stream, WireResponse::Query(response));
}
