use rivu_metadata::{TagPatch, edit_file};

const SERIAL: u32 = 0x1234_5678;

fn crc(bytes: &[u8]) -> u32 {
    let mut value = 0u32;
    for &byte in bytes {
        value ^= u32::from(byte) << 24;
        for _ in 0..8 {
            value = (value << 1)
                ^ if value & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
    }
    value
}

fn page(serial: u32, sequence: u32, flags: u8, granule: u64, packets: &[&[u8]]) -> Vec<u8> {
    let mut segments = Vec::new();
    let mut content = Vec::new();
    for packet in packets {
        segments.extend(std::iter::repeat_n(255, packet.len() / 255));
        segments.push((packet.len() % 255) as u8);
        content.extend_from_slice(packet);
    }
    assert!(segments.len() <= 255);
    let mut bytes = b"OggS\0".to_vec();
    bytes.push(flags);
    bytes.extend_from_slice(&granule.to_le_bytes());
    bytes.extend_from_slice(&serial.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.push(segments.len() as u8);
    bytes.extend(segments);
    bytes.extend(content);
    let checksum = crc(&bytes);
    bytes[22..26].copy_from_slice(&checksum.to_le_bytes());
    bytes
}

fn identification() -> Vec<u8> {
    let mut bytes = b"OpusHead".to_vec();
    bytes.extend_from_slice(&[1, 2, 0, 0]);
    bytes.extend_from_slice(&48000u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes
}

fn comments(title: &str) -> Vec<u8> {
    let mut bytes = b"OpusTags".to_vec();
    bytes.extend_from_slice(&6u32.to_le_bytes());
    bytes.extend_from_slice(b"vendor");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&((6 + title.len()) as u32).to_le_bytes());
    bytes.extend_from_slice(b"TITLE=");
    bytes.extend_from_slice(title.as_bytes());
    bytes
}

fn headers() -> Vec<u8> {
    let mut bytes = page(SERIAL, 0, 2, 0, &[&identification()]);
    bytes.extend(page(SERIAL, 1, 0, 0, &[&comments("Old")]));
    bytes
}

fn pages(bytes: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        assert_eq!(&bytes[position..position + 4], b"OggS");
        let segment_count = usize::from(bytes[position + 26]);
        let header_end = position + 27 + segment_count;
        let content_len: usize = bytes[position + 27..header_end]
            .iter()
            .map(|&n| usize::from(n))
            .sum();
        let end = header_end + content_len;
        let current = &bytes[position..end];
        let mut checked = current.to_vec();
        let expected = u32::from_le_bytes(checked[22..26].try_into().unwrap());
        checked[22..26].fill(0);
        assert_eq!(crc(&checked), expected, "page CRC");
        result.push(current);
        position = end;
    }
    result
}

fn payload(page: &[u8]) -> &[u8] {
    &page[27 + usize::from(page[26])..]
}

#[test]
fn opus_edit_transaction_updates_tags_and_keeps_audio_and_original_backup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("track.opus");
    let mut original = headers();
    let audio = b"encoded audio survives transaction";
    original.extend(page(SERIAL, 2, 4, 960, &[audio]));
    std::fs::write(&path, &original).unwrap();

    let report = edit_file(
        &path,
        &TagPatch {
            title: Some("Updated".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.changed());
    assert_eq!(
        std::fs::read(report.backup_path().unwrap()).unwrap(),
        original
    );
    let saved = std::fs::read(&path).unwrap();
    let parsed = pages(&saved);
    assert_eq!(payload(parsed.last().unwrap()), audio);
    let tags = rivu_metadata::read_tags(&mut std::fs::File::open(path).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(tags.title.as_deref(), Some("Updated"));
}
