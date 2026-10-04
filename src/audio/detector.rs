//! Conservative bounded DTS core-frame detector.
//!
//! A sync word alone is not evidence: three contiguous, structurally valid
//! frames are required. The scanner only examines RIFF `data` payloads (when
//! present), never arbitrary metadata chunks.
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MAX_SCAN: usize = 4 * 1024 * 1024;
const MIN_FRAME: usize = 96;
const MAX_FRAME: usize = 32 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Detection {
    pub(super) codec: &'static str,
    pub(super) sample_rate: Option<u32>,
    // Core AMODE is not the final DTS-ES layout. Keep this unknown so a
    // disabled probe cannot advertise a routing that was not decoded.
    pub(super) channels: Option<u16>,
}

pub(super) fn scan(path: &Path) -> Result<Option<Detection>> {
    let mut file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let mut magic = [0u8; 12];
    file.read_exact(&mut magic).ok();
    if &magic[..4] == b"fLaC" || &magic[..4] == b"OggS" || &magic[4..8] == b"ftyp" {
        return Ok(None);
    }
    if let Some(regions) = riff_data_regions(&mut file)? {
        for (offset, length) in regions {
            if let Some(found) = scan_region(&mut file, offset, length)? {
                return Ok(Some(found));
            }
        }
        return Ok(None);
    }
    let length = file.metadata()?.len();
    scan_region(&mut file, 0, length)
}

fn scan_region(file: &mut File, offset: u64, length: u64) -> Result<Option<Detection>> {
    const CHUNK: usize = 64 * 1024;
    let limit = length.min(MAX_SCAN as u64);
    let mut position = 0u64;
    let mut overlap = Vec::new();
    while position < limit {
        let count = (limit - position).min(CHUNK as u64) as usize;
        file.seek(SeekFrom::Start(offset + position))?;
        let mut chunk = vec![0u8; count];
        file.read_exact(&mut chunk)?;
        let mut window = Vec::with_capacity(overlap.len() + chunk.len());
        window.extend_from_slice(&overlap);
        window.extend_from_slice(&chunk);
        if let Some(found) = scan_bytes(&window) {
            return Ok(Some(found));
        }
        let keep = (MAX_FRAME * 2).min(window.len());
        overlap.clear();
        overlap.extend_from_slice(&window[window.len() - keep..]);
        position += count as u64;
    }
    Ok(None)
}

fn riff_data_regions(file: &mut File) -> Result<Option<Vec<(u64, u64)>>> {
    let mut head = [0u8; 12];
    file.seek(SeekFrom::Start(0))?;
    if file.read_exact(&mut head).is_err() || &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Ok(None);
    }
    let mut regions = Vec::new();
    let mut cursor = 12u64;
    loop {
        let mut chunk = [0u8; 8];
        file.seek(SeekFrom::Start(cursor))?;
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        let size = u64::from(u32::from_le_bytes(chunk[4..8].try_into().unwrap()));
        let body = cursor + 8;
        if &chunk[..4] == b"data" {
            regions.push((body, size));
        }
        cursor = body.saturating_add(size).saturating_add(size & 1);
        if regions.len() >= 4 {
            break;
        }
    }
    Ok(Some(regions))
}

fn sync_kind(bytes: &[u8], at: usize) -> Option<bool> {
    if at + 4 > bytes.len() {
        return None;
    }
    match &bytes[at..at + 4] {
        [0x7f, 0xfe, 0x80, 0x01] | [0xfe, 0x7f, 0x01, 0x80] => Some(false),
        [0x1f, 0xff, 0xe8, 0x00] | [0xff, 0x1f, 0x00, 0xe8] => Some(true),
        _ => None,
    }
}

fn little_word(bytes: &[u8], at: usize) -> bool {
    bytes[at..at + 4] == [0xfe, 0x7f, 0x01, 0x80] || bytes[at..at + 4] == [0xff, 0x1f, 0x00, 0xe8]
}
fn bit_get(bytes: &[u8], bit: usize) -> u32 {
    u32::from((bytes[bit / 8] >> (7 - bit % 8)) & 1)
}
fn bits(bytes: &[u8], start: usize, count: usize) -> u32 {
    (0..count).fold(0, |value, n| (value << 1) | bit_get(bytes, start + n))
}

fn rate(code: u32) -> Option<u32> {
    Some(match code {
        1 => 8_000,
        2 => 16_000,
        3 => 32_000,
        6 => 11_025,
        7 => 22_050,
        8 => 44_100,
        11 => 12_000,
        12 => 24_000,
        13 => 48_000,
        14 => 96_000,
        15 => 192_000,
        _ => return None,
    })
}

fn header(bytes: &[u8], at: usize, fourteen: bool) -> Option<(usize, Option<u32>)> {
    if at + 12 > bytes.len() {
        return None;
    }
    let little = little_word(bytes, at);
    let mut normalized = [0u8; 16];
    let source = &bytes[at..];
    if fourteen {
        // Each stored word has 14 payload bits and two padding bits. LE swaps
        // complete words, not individual bits.
        let mut out = 0usize;
        let mut input = 0usize;
        while input + 1 < source.len() && out < 128 {
            let word = if little {
                u16::from_le_bytes([source[input], source[input + 1]])
            } else {
                u16::from_be_bytes([source[input], source[input + 1]])
            } & 0x3fff;
            for bit in (0..14).rev() {
                if out >= normalized.len() * 8 {
                    break;
                }
                if word & (1 << bit) != 0 {
                    normalized[out / 8] |= 1 << (7 - out % 8);
                }
                out += 1;
            }
            input += 2;
        }
        if out < 80 {
            return None;
        }
        let logical_bytes = bits(&normalized, 46, 14) as usize + 1;
        // In 14-bit mode FSIZE is the unpacked logical byte count minus one.
        // Physical storage uses 14 payload bits in every 16-bit word.
        let frame = logical_bytes.checked_mul(8)?.div_ceil(14).checked_mul(2)?;
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame) {
            return None;
        }
        let sample_rate = rate(bits(&normalized, 66, 4));
        sample_rate.map(|rate| (frame, Some(rate)))
    } else {
        // Normalize LE by swapping each complete 16-bit word, then parse the
        // canonical big-endian bitstream. No per-byte bit reversal is valid.
        let words = source.len().min(normalized.len() / 2 * 2) & !1;
        for pos in (0..words).step_by(2) {
            let pair = if little {
                [source[pos + 1], source[pos]]
            } else {
                [source[pos], source[pos + 1]]
            };
            normalized[pos..pos + 2].copy_from_slice(&pair);
        }
        let size = bits(&normalized, 46, 14) as usize + 1;
        if !(MIN_FRAME..=MAX_FRAME).contains(&size) {
            return None;
        }
        let sample_rate = rate(bits(&normalized, 66, 4));
        sample_rate.map(|rate| (size, Some(rate)))
    }
}

fn scan_bytes(bytes: &[u8]) -> Option<Detection> {
    for at in 0..bytes.len().saturating_sub(16) {
        let Some(fourteen) = sync_kind(bytes, at) else {
            continue;
        };
        let Some((frame, sample_rate)) = header(bytes, at, fourteen) else {
            continue;
        };
        let mut cursor = at + frame;
        let mut count = 1;
        while count < 3 {
            if cursor + 4 > bytes.len() || sync_kind(bytes, cursor) != Some(fourteen) {
                break;
            }
            let Some((next, next_rate)) = header(bytes, cursor, fourteen) else {
                break;
            };
            if next != frame || next_rate != sample_rate {
                break;
            }
            count += 1;
            cursor += next;
        }
        if count == 3 {
            return Some(Detection {
                codec: "DTS",
                sample_rate,
                channels: None,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lone_sync_is_not_detection() {
        let mut bytes = vec![0u8; 512];
        bytes[..4].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01]);
        assert!(scan_bytes(&bytes).is_none());
    }
    #[test]
    fn ordinary_pcm_pattern_is_not_detection() {
        let mut bytes = vec![0u8; 4096];
        bytes[100..104].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01]);
        assert!(scan_bytes(&bytes).is_none());
    }

    fn set_bits(bytes: &mut [u8], start: usize, count: usize, value: u32) {
        for n in 0..count {
            let bit = (value >> (count - n - 1)) & 1;
            bytes[(start + n) / 8] |= (bit as u8) << (7 - (start + n) % 8);
        }
    }
    fn core16(little: bool) -> Vec<u8> {
        let mut frame = vec![0u8; 96 * 3];
        for n in 0..3 {
            let out = &mut frame[n * 96..(n + 1) * 96];
            out[..4].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01]);
            set_bits(out, 46, 14, 95); // 96-byte frame
            set_bits(out, 66, 4, 8); // 44.1 kHz
            if little {
                for pair in out.chunks_exact_mut(2) {
                    pair.swap(0, 1);
                }
            }
        }
        frame
    }
    fn core14(little: bool) -> Vec<u8> {
        let mut frame = vec![0u8; 96 * 3];
        for n in 0..3 {
            let mut unpacked = [0u8; 84];
            unpacked[..4].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01]);
            set_bits(&mut unpacked, 46, 14, 83); // FSIZE+1=84 unpacked bytes = 96 physical
            set_bits(&mut unpacked, 66, 4, 8);
            let out = &mut frame[n * 96..(n + 1) * 96];
            for word_index in 0..48 {
                let mut word = 0u16;
                for bit in 0..14 {
                    word |= (bit_get(&unpacked, word_index * 14 + bit) as u16) << (13 - bit);
                }
                if word & 0x2000 != 0 {
                    word |= 0xc000;
                }
                let bytes = if little {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                };
                out[word_index * 2..word_index * 2 + 2].copy_from_slice(&bytes);
            }
        }
        frame
    }
    #[test]
    fn recognizes_all_dts_core_packings() {
        for bytes in [core16(false), core16(true), core14(false), core14(true)] {
            let found = scan_bytes(&bytes).expect("three valid core frames");
            assert_eq!(found.sample_rate, Some(44_100));
            assert_eq!(found.channels, None);
        }
    }
}
