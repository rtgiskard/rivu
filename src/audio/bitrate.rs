//! Cheap, conservative elementary-stream bitrate summaries on the native open path.
//!
//! Reads only bounded headers/summary fields on the already-open seekable source,
//! then restores its position. Compressed summaries are estimates: no scan-time
//! payload traversal or FLAC tail CRC verification is performed.

use super::source::check_cancelled;
use anyhow::{Context, Result};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};
use symphonia::core::{
    codecs::audio::{
        AudioCodecId,
        well_known::{CODEC_ID_FLAC, CODEC_ID_MP3},
    },
    io::{MediaSource, MediaSourceStream, ReadBytes},
};

const MAX_HEADERS: usize = 128;
// Leading/trailing tag and FLAC metadata header counts are independently bounded.

/// Selected encoded payload bytes / represented encoded duration. MPEG samples
/// come from packet frame headers; other codecs restore decoder-applied trim.
/// No container bytes, decoded PCM sizes or gapless duration are used.
pub(super) struct PacketStats {
    bytes: u64,
    ticks: u64,
    valid: bool,
    eof: bool,
}

impl PacketStats {
    pub(super) fn new() -> Self {
        Self {
            bytes: 0,
            ticks: 0,
            valid: true,
            eof: false,
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.valid = false;
    }

    pub(super) fn add(&mut self, bytes: u64, ticks: u64) {
        if !self.valid {
            return;
        }
        let total = self
            .bytes
            .checked_add(bytes)
            .zip(self.ticks.checked_add(ticks));
        match total {
            Some((bytes_total, ticks_total)) if bytes > 0 && !self.eof => {
                self.bytes = bytes_total;
                self.ticks = ticks_total;
            }
            _ => self.invalidate(),
        }
    }

    #[cfg(feature = "ffmpeg")]
    pub(super) fn add_duration(&mut self, ticks: u64) {
        if !self.valid {
            return;
        }
        match self.ticks.checked_add(ticks) {
            Some(total) if !self.eof => self.ticks = total,
            _ => self.invalidate(),
        }
    }

    pub(super) fn finish(&mut self, expected_ticks: Option<u64>) {
        if expected_ticks.is_some_and(|expected| expected != self.ticks) {
            self.invalidate();
        }
        self.eof = true;
    }

    pub(super) fn bitrate(&self, numer: u32, denom: u32) -> Option<u64> {
        if !self.valid || !self.eof || self.ticks == 0 || numer == 0 || denom == 0 {
            return None;
        }
        let numerator = u128::from(self.bytes)
            .checked_mul(8)?
            .checked_mul(u128::from(denom))?;
        let duration = u128::from(self.ticks).checked_mul(u128::from(numer))?;
        u64::try_from(numerator.checked_add(duration / 2)? / duration)
            .ok()
            .filter(|value| *value > 0)
    }
}

/// Symphonia keeps Vorbis's identification packet in codec extra data.
pub(super) fn vorbis_nominal(header: &[u8]) -> Option<u64> {
    if header.get(..7)? != b"\x01vorbis"
        || header.get(7..11)? != [0; 4]
        || *header.get(11)? == 0
        || header.get(29)? & 1 == 0
    {
        return None;
    }
    let rate = u32::from_le_bytes(header.get(12..16)?.try_into().ok()?);
    let nominal = i32::from_le_bytes(header.get(20..24)?.try_into().ok()?);
    (rate > 0 && nominal > 0).then_some(nominal as u64)
}

#[derive(Debug, PartialEq)]
pub(super) struct Summary {
    pub(super) codec: AudioCodecId,
    pub(super) bitrate: Option<u64>,
    pub(super) expected_samples: Option<u64>,
    pub(super) completion_valid: bool,
}

pub(super) fn probe(
    source: &mut MediaSourceStream<'_>,
    cancelled: Option<&AtomicBool>,
) -> Result<Option<Summary>> {
    check_cancelled(cancelled)?;
    // In particular, never read ahead or seek on a FIFO for optional metadata.
    if !source.is_seekable() {
        return Ok(None);
    }
    let position = source.pos();
    let summary = inspect(source, cancelled);
    source
        .seek(SeekFrom::Start(position))
        .context("Restoring native probe position")?;
    check_cancelled(cancelled)?;
    Ok(summary)
}

fn stopped(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

fn read_at<const N: usize>(source: &mut MediaSourceStream<'_>, offset: u64) -> Option<[u8; N]> {
    source.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = [0; N];
    source.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn synchsafe(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        if byte & 0x80 != 0 {
            None
        } else {
            Some((value << 7) | u64::from(*byte))
        }
    })
}

fn inspect(source: &mut MediaSourceStream<'_>, cancelled: Option<&AtomicBool>) -> Option<Summary> {
    let end = source.byte_len()?;
    let mut start = source.pos();
    for _ in 0..MAX_HEADERS {
        if stopped(cancelled) {
            return None;
        }
        let header = read_at::<10>(source, start)?;
        if &header[..3] != b"ID3" {
            let is_flac = &header[..4] == b"fLaC";
            if !is_flac && mpeg_header(&header).is_none() {
                return None;
            }
            // Unsupported containers never incur optional tail reads/seeks.
            let end = audio_end(source, end, cancelled)?;
            if start >= end {
                return None;
            }
            return if is_flac {
                flac(source, start, end, cancelled).map(|bitrate| Summary {
                    codec: CODEC_ID_FLAC,
                    bitrate: Some(bitrate),
                    expected_samples: None,
                    completion_valid: true,
                })
            } else {
                mp3(source, start, end)
            };
        }
        if !(2..=4).contains(&header[3]) || header[4] == 0xff {
            return None;
        }
        let footer = if header[3] == 4 && header[5] & 0x10 != 0 {
            10
        } else {
            0
        };
        start = start.checked_add(10 + synchsafe(&header[6..10])? + footer)?;
        if start >= end {
            return None;
        }
    }
    None
}

// Strip only self-identifying, size-checked tags. Unknown FLAC tails can affect
// the estimate; only uninterrupted playback can establish a precise average.
fn audio_end(
    source: &mut MediaSourceStream<'_>,
    mut end: u64,
    cancelled: Option<&AtomicBool>,
) -> Option<u64> {
    for _ in 0..MAX_HEADERS {
        if stopped(cancelled) {
            return None;
        }
        if end >= 128 && &read_at::<3>(source, end - 128)? == b"TAG" {
            end -= 128;
            if end >= 227 && &read_at::<4>(source, end - 227)? == b"TAG+" {
                end -= 227;
            }
            continue;
        }
        if end >= 32 {
            let footer = read_at::<32>(source, end - 32)?;
            if &footer[..8] == b"APETAGEX" {
                let version = u32::from_le_bytes(footer[8..12].try_into().ok()?);
                let size = u64::from(u32::from_le_bytes(footer[12..16].try_into().ok()?));
                let flags = u32::from_le_bytes(footer[20..24].try_into().ok()?);
                if !matches!(version, 1000 | 2000)
                    || size < 32
                    || size > end
                    || flags & (1 << 29) != 0
                {
                    return None;
                }
                end -= size;
                if flags & (1 << 31) != 0 {
                    end = end.checked_sub(32)?;
                    let header = read_at::<32>(source, end)?;
                    if header[..20] != footer[..20]
                        || u32::from_le_bytes(header[20..24].try_into().ok()?) & (1 << 29) == 0
                    {
                        return None;
                    }
                }
                continue;
            }
        }
        if end >= 20 {
            let footer = read_at::<10>(source, end - 10)?;
            if &footer[..3] == b"3DI" {
                if footer[3] != 4 || footer[5] & 0x10 == 0 {
                    return None;
                }
                let size = 20_u64.checked_add(synchsafe(&footer[6..10])?)?;
                let start = end.checked_sub(size)?;
                let header = read_at::<10>(source, start)?;
                if &header[..3] != b"ID3" || header[3..] != footer[3..] {
                    return None;
                }
                end = start;
                continue;
            }
        }
        return Some(end);
    }
    None
}

fn average(bytes: u64, samples: u64, rate: u32) -> Option<u64> {
    if bytes == 0 || samples == 0 || rate == 0 {
        return None;
    }
    let numerator = u128::from(bytes) * 8 * u128::from(rate);
    u64::try_from((numerator + u128::from(samples) / 2) / u128::from(samples))
        .ok()
        .filter(|value| *value > 0)
}

#[derive(Clone, Copy)]
struct MpegHeader {
    rate: u32,
    bitrate: u64,
    samples: u64,
    frame_len: usize,
    side_len: usize,
}

fn mpeg_header(bytes: &[u8]) -> Option<MpegHeader> {
    let bits = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    // Layer III only. Free-format cannot provide a header bitrate.
    if bits & 0xffe0_0000 != 0xffe0_0000 || (bits >> 17) & 3 != 1 || bits & 3 == 2 {
        return None;
    }
    let version = (bits >> 19) & 3;
    let divisor = match version {
        3 => 1,
        2 => 2,
        0 => 4,
        _ => return None,
    };
    let rate = match (bits >> 10) & 3 {
        0 => 44_100,
        1 => 48_000,
        2 => 32_000,
        _ => return None,
    } / divisor;
    let index = ((bits >> 12) & 15) as usize;
    let kbps = if version == 3 {
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ][index]
    } else {
        [
            0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ][index]
    };
    if kbps == 0 {
        return None;
    }
    let bitrate = kbps * 1000;
    let samples = if version == 3 { 1152 } else { 576 };
    let frame_len =
        ((samples / 8 * bitrate) / u64::from(rate) + u64::from((bits >> 9) & 1)) as usize;
    let mono = (bits >> 6) & 3 == 3;
    let side_len = match (version == 3, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    };
    Some(MpegHeader {
        rate,
        bitrate,
        samples,
        frame_len,
        side_len,
    })
}

/// The native MPEG demuxer emits one complete frame per packet. Reject changed
/// rates, partial frames and unsupported/free-format headers, without inspecting
/// payload or trusting estimated packet trim requests.
pub(super) fn mpeg_packet_samples(bytes: &[u8], rate: u32) -> Option<u64> {
    let header = mpeg_header(bytes)?;
    (header.rate == rate && header.frame_len == bytes.len()).then_some(header.samples)
}

fn be32(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    )))
}

fn be16(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from(u16::from_be_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    )))
}

fn mpeg_average(bytes: u64, frames: u64, header: MpegHeader) -> Option<u64> {
    let (min_bitrate, max_bitrate) = if header.samples == 1152 {
        (32_000, 320_000)
    } else {
        (8000, 160_000)
    };
    let min_frame = header.samples / 8 * min_bitrate / u64::from(header.rate);
    let max_frame = header.samples / 8 * max_bitrate / u64::from(header.rate) + 1;
    if frames == 0
        || bytes < frames.checked_mul(min_frame)?
        || bytes > frames.checked_mul(max_frame)?
    {
        return None;
    }
    average(bytes, frames.checked_mul(header.samples)?, header.rate)
}

fn mp3(source: &mut MediaSourceStream<'_>, start: u64, end: u64) -> Option<Summary> {
    let header = mpeg_header(&read_at::<4>(source, start)?)?;
    if header.frame_len > 1441 || start.checked_add(header.frame_len as u64)? >= end {
        return None;
    }
    let mut frame = [0; 1441];
    source.seek(SeekFrom::Start(start)).ok()?;
    source.read_exact(&mut frame[..header.frame_len]).ok()?;
    let frame = &frame[..header.frame_len];
    // Xing's conventional offset ignores the optional MPEG CRC, as do the
    // native demuxer and common encoders. This frame carries tags, not samples.
    let offset = 4 + header.side_len;
    if let Some(id) = frame
        .get(offset..offset + 4)
        .filter(|id| *id == b"Xing" || *id == b"Info")
    {
        let flags = be32(frame, offset + 4)?;
        if flags & !15 != 0 {
            return None;
        }
        let mut pos = offset + 8;
        let frames = if flags & 1 != 0 {
            let count = be32(frame, pos)?;
            pos += 4;
            Some(count)
        } else {
            None
        };
        let bytes = if flags & 2 != 0 {
            let count = be32(frame, pos)?;
            pos += 4;
            Some(count)
        } else {
            None
        };
        if flags & 4 != 0 {
            pos += 100;
        }
        if flags & 8 != 0 {
            pos += 4;
        }
        if pos > frame.len() || frames == Some(0) || bytes == Some(0) {
            return None;
        }
        // Validate the summary against its own declared representation, not the
        // current file length: truncation must not erase completion authority.
        let declared_average = bytes
            .and_then(|bytes| bytes.checked_sub(header.frame_len as u64))
            .zip(frames)
            .and_then(|(bytes, frames)| mpeg_average(bytes, frames, header));
        if bytes.is_some() && frames.is_some() && declared_average.is_none() {
            return None;
        }
        let completion_valid = bytes.is_none_or(|bytes| bytes == end - start);
        let next = read_at::<4>(source, start + header.frame_len as u64)
            .and_then(|bytes| mpeg_header(&bytes))
            .filter(|next| next.rate == header.rate);
        let bitrate = if !completion_valid || next.is_none() {
            None
        } else if id == b"Info" {
            next.filter(|next| next.bitrate == header.bitrate)
                .map(|_| header.bitrate)
        } else {
            declared_average
        };
        // Xing counts exclude its tag-only frame, whereas its byte count
        // includes that frame. Never use the demuxer's trimmed/estimated count.
        return Some(Summary {
            codec: CODEC_ID_MP3,
            bitrate,
            expected_samples: frames.and_then(|frames| frames.checked_mul(header.samples)),
            completion_valid,
        });
    }
    // VBRI is embedded in the first audio frame, at a fixed header-relative
    // offset, and its frame/byte counts both include that frame.
    if frame.get(36..40) == Some(b"VBRI".as_slice()) {
        if be16(frame, 40)? != 1 {
            return None;
        }
        let bytes = be32(frame, 46)?;
        let frames = be32(frame, 50)?;
        let entries = be16(frame, 54)?;
        let scale = be16(frame, 56)?;
        let entry_bytes = be16(frame, 58)?;
        let entry_frames = be16(frame, 60)?;
        if entries == 0
            || scale == 0
            || !(1..=4).contains(&entry_bytes)
            || entry_frames == 0
            || entries * entry_frames < frames
            || (entries - 1) * entry_frames >= frames
        {
            return None;
        }
        let toc = frame.get(62..62 + (entries * entry_bytes) as usize)?;
        if toc
            .chunks_exact(entry_bytes as usize)
            .any(|entry| entry.iter().all(|byte| *byte == 0))
        {
            return None;
        }
        let bitrate = mpeg_average(bytes, frames, header)?;
        let completion_valid = bytes == end - start;
        return Some(Summary {
            codec: CODEC_ID_MP3,
            bitrate: completion_valid.then_some(bitrate),
            expected_samples: frames.checked_mul(header.samples),
            completion_valid,
        });
    }
    // An ordinary first-frame rate is only an estimate, even when the next
    // header agrees. Never infer CBR/exactness from a sampled prefix.
    let next = mpeg_header(&read_at::<4>(source, start + header.frame_len as u64)?)?;
    (next.rate == header.rate).then_some(Summary {
        codec: CODEC_ID_MP3,
        bitrate: Some(header.bitrate),
        expected_samples: None,
        completion_valid: true,
    })
}

fn flac(
    source: &mut MediaSourceStream<'_>,
    start: u64,
    end: u64,
    cancelled: Option<&AtomicBool>,
) -> Option<u64> {
    let mut pos = start.checked_add(4)?;
    let mut stream_info = None;
    for index in 0..MAX_HEADERS {
        if stopped(cancelled) {
            return None;
        }
        let header = read_at::<4>(source, pos)?;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as u64;
        let kind = header[0] & 127;
        pos = pos.checked_add(4)?;
        if pos.checked_add(len)? > end || kind == 127 {
            return None;
        }
        if index == 0 {
            if kind != 0 || len != 34 {
                return None;
            }
            stream_info = Some(read_at::<34>(source, pos)?);
        } else if kind == 0 {
            return None;
        }
        pos += len;
        if header[0] & 128 == 0 {
            continue;
        }
        let info = stream_info?;
        let packed = u64::from_be_bytes(info[10..18].try_into().ok()?);
        let rate = (packed >> 44) as u32;
        let samples = packed & ((1_u64 << 36) - 1);
        // Skip metadata bodies, including pictures; do not inspect audio frames.
        return average(end.checked_sub(pos)?, samples, rate);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use symphonia::core::io::{MediaSourceStreamOptions, ReadOnlySource};

    fn summary(bytes: Vec<u8>) -> Option<(AudioCodecId, u64)> {
        let mut source = MediaSourceStream::new(
            Box::new(Cursor::new(bytes)),
            MediaSourceStreamOptions::default(),
        );
        let result = probe(&mut source, None)
            .unwrap()
            .and_then(|summary| summary.bitrate.map(|bitrate| (summary.codec, bitrate)));
        assert_eq!(
            source.pos(),
            0,
            "optional probe must not consume the demuxer's stream"
        );
        result
    }

    fn id3(payload: usize) -> Vec<u8> {
        let mut bytes = b"ID3\x04\0\0\0\0\0\0".to_vec();
        for (index, byte) in bytes[6..10].iter_mut().enumerate() {
            *byte = ((payload >> ((3 - index) * 7)) & 127) as u8;
        }
        bytes.resize(10 + payload, 0);
        bytes
    }

    fn ape(payload: usize, with_header: bool) -> Vec<u8> {
        let mut footer = [0; 32];
        footer[..8].copy_from_slice(b"APETAGEX");
        footer[8..12].copy_from_slice(&2000_u32.to_le_bytes());
        footer[12..16].copy_from_slice(&((payload + 32) as u32).to_le_bytes());
        let flags = if with_header { 1_u32 << 31 } else { 0 };
        footer[20..24].copy_from_slice(&flags.to_le_bytes());
        let mut bytes = Vec::new();
        if with_header {
            let mut header = footer;
            header[20..24].copy_from_slice(&(flags | (1 << 29)).to_le_bytes());
            bytes.extend_from_slice(&header);
        }
        bytes.resize(bytes.len() + payload, 0);
        bytes.extend_from_slice(&footer);
        bytes
    }

    fn flac_frame(_number: u8, payload: usize) -> Vec<u8> {
        // Payload is intentionally opaque: cheap summaries never validate frames.
        vec![0; 12 + payload]
    }

    fn flac_file(picture_bytes: usize, frame_padding: usize) -> (Vec<u8>, u64) {
        let mut info = [0; 34];
        info[..2].copy_from_slice(&16_u16.to_be_bytes());
        info[2..4].copy_from_slice(&16_u16.to_be_bytes());
        info[10..18].copy_from_slice(&((8000_u64 << 44) | (15 << 36) | 32).to_be_bytes());
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[if picture_bytes == 0 { 128 } else { 0 }, 0, 0, 34]);
        bytes.extend_from_slice(&info);
        if picture_bytes > 0 {
            let len = (picture_bytes as u32).to_be_bytes();
            bytes.extend_from_slice(&[0x86, len[1], len[2], len[3]]);
            bytes.resize(bytes.len() + picture_bytes, 0);
        }
        let first = flac_frame(0, 0);
        let last = flac_frame(1, frame_padding);
        let audio_bytes = (first.len() + last.len()) as u64;
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&last);
        (bytes, audio_bytes)
    }

    fn mpeg_frame(kbps_index: u8) -> Vec<u8> {
        let header = [0xff, 0xfb, kbps_index << 4, 0];
        let mut bytes = vec![0; mpeg_header(&header).unwrap().frame_len];
        bytes[..4].copy_from_slice(&header);
        bytes
    }

    fn xing_file(id: &[u8; 4]) -> (Vec<u8>, u64) {
        let mut tag = mpeg_frame(9);
        let mut audio = mpeg_frame(9);
        audio.extend_from_slice(&mpeg_frame(if id == b"Info" { 9 } else { 10 }));
        let bytes = (tag.len() + audio.len()) as u32;
        tag[36..40].copy_from_slice(id);
        tag[40..44].copy_from_slice(&3_u32.to_be_bytes());
        tag[44..48].copy_from_slice(&2_u32.to_be_bytes());
        tag[48..52].copy_from_slice(&bytes.to_be_bytes());
        let audio_bytes = audio.len() as u64;
        tag.extend_from_slice(&audio);
        (tag, audio_bytes)
    }

    #[test]
    fn mpeg_packets_use_header_samples_and_require_exact_frame_boundaries() {
        for header in [
            [0xff, 0xfb, 0x90, 0],
            [0xff, 0xf3, 0x90, 0],
            [0xff, 0xe3, 0x90, 0],
        ] {
            let parsed = mpeg_header(&header).unwrap();
            let mut packet = vec![0; parsed.frame_len];
            packet[..4].copy_from_slice(&header);
            let expected = if header[1] == 0xfb { 1152 } else { 576 };
            assert_eq!(mpeg_packet_samples(&packet, parsed.rate), Some(expected));
            assert_eq!(mpeg_packet_samples(&packet, parsed.rate + 1), None);
            packet.pop();
            assert_eq!(mpeg_packet_samples(&packet, parsed.rate), None);
            packet.extend_from_slice(&[0, 0]);
            assert_eq!(mpeg_packet_samples(&packet, parsed.rate), None);
        }
        assert_eq!(mpeg_packet_samples(&[0xff, 0xfb, 0, 0], 44_100), None);
    }

    #[test]
    fn unsupported_headers_after_id3_never_probe_the_tail() {
        struct HeaderOnly {
            bytes: Cursor<Vec<u8>>,
            opening_end: u64,
        }
        impl Read for HeaderOnly {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    self.bytes.position() < self.opening_end,
                    "unexpected tail read"
                );
                let len = buffer
                    .len()
                    .min((self.opening_end - self.bytes.position()) as usize);
                self.bytes.read(&mut buffer[..len])
            }
        }
        impl Seek for HeaderOnly {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                let position = self.bytes.seek(position)?;
                assert!(position <= self.opening_end, "unexpected tail seek");
                Ok(position)
            }
        }
        impl MediaSource for HeaderOnly {
            fn is_seekable(&self) -> bool {
                true
            }
            fn byte_len(&self) -> Option<u64> {
                Some(self.bytes.get_ref().len() as u64)
            }
        }
        for header in [
            b"OggS\0\0\0\0\0\0",
            b"RIFF\0\0\0\0WA",
            b"\0\0\0\0ftypis",
            b"\x1a\x45\xdf\xa3\0\0\0\0\0\0",
        ] {
            for leading_id3 in [false, true] {
                let mut bytes = if leading_id3 { id3(123) } else { Vec::new() };
                bytes.extend_from_slice(header);
                let opening_end = bytes.len() as u64;
                bytes.resize(100_000, 0);
                let mut source = MediaSourceStream::new(
                    Box::new(HeaderOnly {
                        bytes: Cursor::new(bytes),
                        opening_end,
                    }),
                    MediaSourceStreamOptions::default(),
                );
                assert_eq!(probe(&mut source, None).unwrap(), None);
                assert_eq!(source.pos(), 0);
            }
        }
    }

    #[test]
    fn averages_are_audio_bits_per_second_and_round_without_overflow() {
        assert_eq!(average(1000, 8000, 8000), Some(8000));
        assert_eq!(average(1, 3, 1), Some(3));
        assert_eq!(average(0, 1, 1), None);
        assert_eq!(average(1, 0, 1), None);
        assert_eq!(average(1, 1, 0), None);
        assert_eq!(average(u64::MAX, 1, u32::MAX), None);
    }

    #[test]
    fn flac_uses_frames_not_metadata_artwork_or_trailing_tags() {
        let (audio, audio_bytes) = flac_file(300_000, 0);
        let mut bytes = id3(100_000);
        bytes.extend_from_slice(&audio);
        bytes.extend_from_slice(&ape(200_000, true));
        let mut tag = [0; 128];
        tag[..3].copy_from_slice(b"TAG");
        bytes.extend_from_slice(&tag);
        assert_eq!(
            summary(bytes),
            Some((CODEC_ID_FLAC, audio_bytes * 8 * 8000 / 32))
        );
    }

    #[test]
    fn flac_estimates_do_not_claim_tail_integrity_and_require_sample_count() {
        let (mut bytes, audio_bytes) = flac_file(0, 0);
        bytes.extend_from_slice(b"unknown trailing data");
        assert_eq!(
            summary(bytes),
            Some((CODEC_ID_FLAC, average(audio_bytes + 21, 32, 8000).unwrap()))
        );
        let (mut zero, _) = flac_file(0, 0);
        zero[21] &= 0xf0;
        zero[22..26].fill(0);
        assert_eq!(summary(zero), None);
    }

    #[test]
    fn xing_average_excludes_tags_and_uses_all_encoded_audio_samples() {
        let (mp3, audio_bytes) = xing_file(b"Xing");
        let mut bytes = id3(60_000);
        bytes.extend_from_slice(&mp3);
        bytes.extend_from_slice(&ape(30_000, false));
        assert_eq!(
            summary(bytes),
            Some((CODEC_ID_MP3, average(audio_bytes, 2304, 44_100).unwrap()))
        );
    }

    #[test]
    fn explicit_cbr_info_uses_encoded_header_bitrate() {
        assert_eq!(summary(xing_file(b"Info").0), Some((CODEC_ID_MP3, 128_000)));
        let mut unmarked = mpeg_frame(9);
        unmarked.extend_from_slice(&mpeg_frame(9));
        assert_eq!(
            summary(unmarked),
            Some((CODEC_ID_MP3, 128_000)),
            "a sampled frame rate is only an estimate"
        );
    }

    #[test]
    fn malformed_or_incomplete_xing_summaries_are_not_estimates() {
        let (bytes, _) = xing_file(b"Xing");
        for offset in [40, 44, 48] {
            let mut invalid = bytes.clone();
            invalid[offset..offset + 4].fill(0);
            assert_eq!(summary(invalid), None);
        }
        let mut truncated = bytes.clone();
        truncated.pop();
        assert_eq!(summary(truncated), None);
        let mut trailing_junk = bytes;
        trailing_junk.push(0);
        assert_eq!(summary(trailing_junk), None);
    }

    #[test]
    fn vbri_average_uses_complete_declared_frame_and_byte_counts() {
        let mut bytes = mpeg_frame(9);
        bytes.extend_from_slice(&mpeg_frame(10));
        let size = bytes.len() as u32;
        bytes[36..40].copy_from_slice(b"VBRI");
        bytes[40..42].copy_from_slice(&1_u16.to_be_bytes());
        bytes[46..50].copy_from_slice(&size.to_be_bytes());
        bytes[50..54].copy_from_slice(&2_u32.to_be_bytes());
        bytes[54..56].copy_from_slice(&1_u16.to_be_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_be_bytes());
        bytes[58..60].copy_from_slice(&4_u16.to_be_bytes());
        bytes[60..62].copy_from_slice(&2_u16.to_be_bytes());
        bytes[62..66].copy_from_slice(&size.to_be_bytes());
        assert_eq!(
            summary(bytes.clone()),
            Some((
                CODEC_ID_MP3,
                average(u64::from(size), 2304, 44_100).unwrap()
            ))
        );
        bytes[46..50].copy_from_slice(&(size + 1).to_be_bytes());
        assert_eq!(summary(bytes), None);
    }

    #[test]
    fn malformed_metadata_lengths_and_excessive_headers_are_bounded() {
        let mut invalid = id3(0);
        invalid[6] = 128;
        assert_eq!(summary(invalid), None);
        let mut repeated = Vec::new();
        for _ in 0..=MAX_HEADERS {
            repeated.extend_from_slice(&id3(0));
        }
        repeated.extend_from_slice(&flac_file(0, 0).0);
        assert_eq!(summary(repeated), None);
        let mut oversized = b"fLaC\x80\xff\xff\xff".to_vec();
        oversized.resize(64, 0);
        assert_eq!(summary(oversized), None);
    }

    #[test]
    fn nonseekable_sources_are_not_read_and_cancellation_is_reported() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("optional bitrate probe must not read a FIFO")
            }
        }
        let mut fifo = MediaSourceStream::new(
            Box::new(ReadOnlySource::new(NoRead)),
            MediaSourceStreamOptions::default(),
        );
        assert_eq!(probe(&mut fifo, None).unwrap(), None);
        let cancelled = AtomicBool::new(true);
        assert!(probe(&mut fifo, Some(&cancelled)).is_err());
    }
}

#[cfg(test)]
mod packet_tests {
    use super::*;

    #[test]
    fn exact_average_requires_complete_positive_checked_accounting() {
        let mut stats = PacketStats::new();
        assert_eq!(stats.bitrate(1, 48_000), None);
        stats.add(100, 960);
        stats.add(300, 960);
        assert_eq!(
            stats.bitrate(1, 48_000),
            None,
            "decode prefix is not complete"
        );
        stats.finish(Some(1920));
        assert_eq!(stats.bitrate(1, 48_000), Some(80_000));
        stats.invalidate();
        assert_eq!(stats.bitrate(1, 48_000), None);

        let mut overflow = PacketStats::new();
        overflow.add(u64::MAX, 1);
        overflow.add(1, 1);
        overflow.finish(None);
        assert_eq!(overflow.bitrate(1, 1), None);
        let mut short = PacketStats::new();
        short.add(20, 10);
        short.finish(Some(11));
        assert_eq!(short.bitrate(1, 1), None);
        let mut empty = PacketStats::new();
        empty.finish(None);
        assert_eq!(empty.bitrate(1, 1), None);
        let mut missing_duration = PacketStats::new();
        missing_duration.add(20, 0);
        missing_duration.finish(None);
        assert_eq!(missing_duration.bitrate(1, 1), None);
    }

    #[test]
    fn vorbis_positive_nominal_is_an_estimate_and_opus_has_no_nominal() {
        let mut header = [0; 30];
        header[..7].copy_from_slice(b"\x01vorbis");
        header[11] = 2;
        header[12..16].copy_from_slice(&44_100_u32.to_le_bytes());
        header[20..24].copy_from_slice(&160_000_i32.to_le_bytes());
        header[28] = 0xb8;
        header[29] = 1;
        assert_eq!(vorbis_nominal(&header), Some(160_000));
        header[20..24].copy_from_slice(&(-1_i32).to_le_bytes());
        assert_eq!(vorbis_nominal(&header), None);
        assert_eq!(vorbis_nominal(b"OpusHead"), None);
        assert_eq!(vorbis_nominal(&[]), None);
    }
}
