//! Deterministic, in-memory SF2 and SMF fixtures; never consult the process bank.

use super::*;
use std::io::Seek;

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

pub(super) fn bank() -> Arc<SoundFont> {
    let mut info = b"INFO".to_vec();
    chunk(&mut info, b"ifil", &[2, 0, 1, 0]);
    chunk(&mut info, b"isng", b"EMU8000\0");
    chunk(&mut info, b"INAM", b"Fixture\0");

    // A long, ordinary loop exercises actual sustained synthesis. RustySynth
    // 1.3.8's short-loop/high-pitch defect is explicitly outside these tests;
    // it is not caught, patched, or avoided in production admission.
    const LENGTH: u32 = 4096;
    let mut samples = Vec::new();
    for i in 0..LENGTH + 46 {
        let phase = (i % 128) as i16;
        let sample = if phase < 64 { phase - 32 } else { 96 - phase } * 500;
        samples.extend_from_slice(&sample.to_le_bytes());
    }
    let mut sdta = b"sdta".to_vec();
    chunk(&mut sdta, b"smpl", &samples);

    let mut pdta = b"pdta".to_vec();
    let mut presets = vec![0; 76];
    presets[..7].copy_from_slice(b"Fixture");
    presets[38..41].copy_from_slice(b"EOP");
    presets[62..64].copy_from_slice(&1_u16.to_le_bytes());
    chunk(&mut pdta, b"phdr", &presets);
    chunk(&mut pdta, b"pbag", &[0, 0, 0, 0, 1, 0, 0, 0]);
    chunk(&mut pdta, b"pmod", &[0; 10]);
    chunk(&mut pdta, b"pgen", &[41, 0, 0, 0, 0, 0, 0, 0]);
    let mut instruments = vec![0; 44];
    instruments[..7].copy_from_slice(b"Fixture");
    instruments[22..25].copy_from_slice(b"EOI");
    instruments[42..44].copy_from_slice(&1_u16.to_le_bytes());
    chunk(&mut pdta, b"inst", &instruments);
    chunk(&mut pdta, b"ibag", &[0, 0, 0, 0, 3, 0, 0, 0]);
    chunk(&mut pdta, b"imod", &[0; 10]);
    let mut generators = Vec::new();
    for (kind, value) in [(38_u16, -1200_i16), (54, 1), (53, 0), (0, 0)] {
        generators.extend_from_slice(&kind.to_le_bytes());
        generators.extend_from_slice(&value.to_le_bytes());
    }
    chunk(&mut pdta, b"igen", &generators);
    let mut headers = vec![0; 92];
    headers[..7].copy_from_slice(b"Fixture");
    headers[24..28].copy_from_slice(&LENGTH.to_le_bytes());
    headers[32..36].copy_from_slice(&LENGTH.to_le_bytes());
    headers[36..40].copy_from_slice(&RATE.to_le_bytes());
    headers[40] = 60;
    headers[44..46].copy_from_slice(&1_u16.to_le_bytes());
    headers[46..49].copy_from_slice(b"EOS");
    chunk(&mut pdta, b"shdr", &headers);

    let mut riff = b"sfbk".to_vec();
    for list in [info, sdta, pdta] {
        chunk(&mut riff, b"LIST", &list);
    }
    let mut bytes = Vec::new();
    chunk(&mut bytes, b"RIFF", &riff);
    let mut reader = std::io::Cursor::new(bytes);
    let len = reader.get_ref().len() as u64;
    validate_bank_chunks(&mut reader, len).unwrap();
    reader.rewind().unwrap();
    Arc::new(SoundFont::new(&mut reader).unwrap())
}

fn delta(out: &mut Vec<u8>, mut value: u32) {
    let mut bytes = [0; 4];
    let mut index = 3;
    bytes[index] = (value & 127) as u8;
    while {
        value >>= 7;
        value != 0
    } {
        index -= 1;
        bytes[index] = (value & 127) as u8 | 128;
    }
    out.extend_from_slice(&bytes[index..]);
}

pub(super) fn smf(events: &[(u32, &[u8])]) -> Vec<u8> {
    let mut track = Vec::new();
    for &(ticks, message) in events {
        delta(&mut track, ticks);
        track.extend_from_slice(message);
    }
    let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk".to_vec();
    bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&track);
    bytes
}

pub(in crate::source) fn held_note() -> Vec<u8> {
    smf(&[
        (0, &[0xb0, 91, 96]),
        (0, &[0xb0, 93, 80]),
        (0, &[0x90, 60, 100]),
        (240, &[0xb0, 64, 127]),
        (240, &[0x80, 60, 0]),
        (240, &[0xb0, 11, 80]),
        (240, &[0xff, 0x2f, 0]),
    ])
}

pub(in crate::source) fn source(bytes: &[u8]) -> MidiSource {
    let mut events = Vec::new();
    let timeline = Timeline::parse(bytes, Some(&mut events), None).unwrap();
    let sequence = Sequence {
        events,
        timeline,
        identity: SourceIdentity::File {
            device: 0,
            inode: 1,
            len: bytes.len() as u64,
            modified: (0, 0),
            changed: (0, 0),
        },
    };
    let bank = Arc::new(SoundFontLoad::completed(bank()));
    let mut source = MidiSource::with_bank_load(sequence, bank);
    assert!(source.poll_open().unwrap());
    source
}

pub(in crate::source) fn audio_source(bytes: &[u8]) -> super::super::Source {
    let midi = source(bytes);
    let info = midi.sequence.info(Path::new("fixture.mid"));
    super::super::Source::from_midi(midi.sequence.identity, info, midi)
}
