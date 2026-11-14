use super::*;
use std::fs::File;

pub(super) use crate::fixtures::PNG_IMAGE as DEFAULT_IMAGE;

pub fn tagged_wav(path: &Path) {
    tagged_wav_image(path, DEFAULT_IMAGE);
}

pub(super) fn tagged_wav_image(path: &Path, image: &[u8]) {
    pictures_wav(path, 3, &[(image, image.len(), 3)]);
}

fn syncsafe(value: usize) -> [u8; 4] {
    std::array::from_fn(|index| ((value >> (21 - 7 * index)) & 0x7f) as u8)
}

#[cfg(test)]
pub(super) fn write_test_wave(file: &mut File) {
    use std::io::Write;
    file.write_all(b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x02\0\0\0\0\x40").unwrap();
}

fn begin_test_wave_id3(file: &mut File, tag_len: usize) {
    // RIFF sizes include the ID3 header and even-byte chunk padding. Keep
    // metadata before data so native audio readers must skip it correctly.
    use std::io::Write;
    file.write_all(b"RIFF").unwrap();
    file.write_all(&((46 + tag_len + tag_len % 2) as u32).to_le_bytes())
        .unwrap();
    file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0id3 ")
        .unwrap();
    file.write_all(&(tag_len as u32).to_le_bytes()).unwrap();
}

fn finish_test_wave(file: &mut File, tag_len: usize) {
    use std::io::Write;
    if !tag_len.is_multiple_of(2) {
        file.write_all(&[0]).unwrap();
    }
    file.write_all(b"data\x02\0\0\0\0\x40").unwrap();
}

// Stream large fixture payloads without creating large test buffers.
pub fn pictures_wav(path: &Path, version: u8, pictures: &[(&[u8], usize, u8)]) {
    use std::io::{Read, Write};
    let header_len = if version == 2 { 6 } else { 10 };
    // Eight hexadecimal description bytes keep every picture distinct;
    // identical frames otherwise collapse in Lofty's retained frame list.
    let picture_prefix = if version == 2 { 6 } else { 13 } + 8;
    let title = b"\0Budget title";
    let tag_len = pictures
        .iter()
        .map(|(_, size, _)| header_len + picture_prefix + size)
        .sum::<usize>()
        + header_len
        + title.len();
    let mut file = File::create(path).unwrap();
    begin_test_wave_id3(&mut file, 10 + tag_len);
    file.write_all(&[b'I', b'D', b'3', version, 0, 0]).unwrap();
    file.write_all(&syncsafe(tag_len)).unwrap();
    let frame_header = |file: &mut File, legacy: &[u8; 3], modern: &[u8; 4], size: usize| {
        if version == 2 {
            file.write_all(legacy).unwrap();
            file.write_all(&(size as u32).to_be_bytes()[1..]).unwrap();
        } else {
            file.write_all(modern).unwrap();
            file.write_all(&if version == 4 {
                syncsafe(size)
            } else {
                (size as u32).to_be_bytes()
            })
            .unwrap();
            file.write_all(&[0, 0]).unwrap();
        }
    };
    for (index, &(bytes, size, usage)) in pictures.iter().enumerate() {
        assert!(size >= bytes.len());
        frame_header(&mut file, b"PIC", b"APIC", picture_prefix + size);
        if version == 2 {
            file.write_all(b"\0PNG").unwrap();
        } else {
            file.write_all(b"\0image/png\0").unwrap();
        }
        file.write_all(&[usage]).unwrap();
        write!(file, "{index:08x}").unwrap();
        file.write_all(&[0]).unwrap();
        file.write_all(bytes).unwrap();
        std::io::copy(
            &mut std::io::repeat(0).take((size - bytes.len()) as u64),
            &mut file,
        )
        .unwrap();
    }
    frame_header(&mut file, b"TT2", b"TIT2", title.len());
    file.write_all(title).unwrap();
    finish_test_wave(&mut file, 10 + tag_len);
}

pub fn budget_unsynchronised_wav(path: &Path, version: u8, tag_wide: bool) {
    use std::io::Write;
    fn unsynchronise(bytes: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        for &byte in bytes {
            encoded.push(byte);
            if byte == 0xff {
                encoded.push(0);
            }
        }
        encoded
    }
    let mut picture = if version == 2 {
        b"\0PNG\x03\0".to_vec()
    } else {
        b"\0image/png\0\x03\0".to_vec()
    };
    picture.extend_from_slice(DEFAULT_IMAGE);
    let picture = if version == 4 {
        unsynchronise(&picture)
    } else {
        picture
    };
    let mut body = if version == 2 {
        b"PIC".to_vec()
    } else {
        b"APIC".to_vec()
    };
    if version == 2 {
        body.extend_from_slice(&(picture.len() as u32).to_be_bytes()[1..]);
    } else {
        body.extend_from_slice(&if version == 4 {
            syncsafe(picture.len())
        } else {
            (picture.len() as u32).to_be_bytes()
        });
        body.extend_from_slice(&[0, if version == 4 && !tag_wide { 2 } else { 0 }]);
    }
    body.extend_from_slice(&picture);
    let body = if version < 4 {
        unsynchronise(&body)
    } else {
        body
    };
    let mut file = File::create(path).unwrap();
    begin_test_wave_id3(&mut file, 10 + body.len());
    file.write_all(&[
        b'I',
        b'D',
        b'3',
        version,
        0,
        if version < 4 || tag_wide { 0x80 } else { 0 },
    ])
    .unwrap();
    file.write_all(&syncsafe(body.len())).unwrap();
    file.write_all(&body).unwrap();
    finish_test_wave(&mut file, 10 + body.len());
}

#[cfg(feature = "test-support")]
/// Hold a real singleflight probe until the test drops its release sender.
pub fn block_probe(
    manager: Arc<ArtworkManager>,
    source: Source<'_>,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let identity = ArtworkIdentity::new(source);
    let (started, entered) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let source = Source {
            id: identity.id,
            fingerprint: identity.fingerprint.as_deref(),
            path: &identity.path,
        };
        let _ = manager.load_with(source, || {
            started.send(()).unwrap();
            let _ = released.recv();
            manager.source_uri(source, manager.max_image_bytes)
        });
    });
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    (release, worker)
}
