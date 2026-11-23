//! Standard MIDI file timing and bounded SoundFont synthesis.

mod bank;

#[cfg(test)]
pub(super) mod fixture;
#[cfg(test)]
mod synthesis_tests;

use super::{check_cancelled, finite_sample};
use crate::MediaInfo;
use anyhow::{Context, Result, ensure};
#[cfg(test)]
use bank::validate_bank_chunks;
use bank::{BANK, SoundFontLoad};
use crossbeam_channel::Receiver;
use midly::{Format, Fps, MetaMessage, MidiMessage, Timing, TrackEventKind};
use rivu_protocol::audio::SourceIdentity;
use rivu_utils::regular_file::RegularFile;
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::FileExt,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

pub(super) const RATE: u32 = 48_000;
const BLOCK: usize = 32;
const FRAGMENT: usize = 1024;
const TAIL: u64 = RATE as u64;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENTS: usize = 500_000;
const MAX_FRAMES: u64 = 4 * 60 * 60 * RATE as u64;
const DEFAULT_TEMPO: u32 = 500_000;

pub(super) fn recognizes(file: &File) -> std::io::Result<bool> {
    let mut header = [0; 4];
    match file.read_exact_at(&mut header, 0) {
        Ok(()) => Ok(&header == b"MThd"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
enum Event {
    Tempo(u32),
    Midi(u8, MidiMessage),
}

struct Clock {
    denominator: u128,
    // Sample-frame numerator per tick, at the current tempo or SMPTE rate.
    step: u128,
    metrical: bool,
    now: u128,
    tick: u64,
}

impl Clock {
    fn new(timing: Timing) -> Result<Self> {
        let (denominator, step, metrical) = match timing {
            Timing::Metrical(ppq) => (
                u128::from(ppq.as_int()) * 1_000_000,
                u128::from(DEFAULT_TEMPO) * u128::from(RATE),
                true,
            ),
            Timing::Timecode(fps, ticks) => {
                let (fps_num, fps_den) = if fps == Fps::Fps29 {
                    (30_000, 1001)
                } else {
                    (u128::from(fps.as_int()), 1)
                };
                (
                    fps_num * u128::from(ticks),
                    u128::from(RATE) * fps_den,
                    false,
                )
            }
        };
        ensure!(denominator != 0, "Invalid MIDI clock division");
        Ok(Self {
            denominator,
            step,
            metrical,
            now: 0,
            tick: 0,
        })
    }

    fn frame(&mut self, tick: u64) -> Result<u64> {
        self.now += u128::from(tick - self.tick) * self.step;
        self.tick = tick;
        // RustySynth dispatches on synthesis-block boundaries. Round forward so
        // an event never sounds before its nominal time.
        let block = self.denominator * BLOCK as u128;
        let frame = self.now.div_ceil(block) * BLOCK as u128;
        ensure!(
            frame <= u128::from(MAX_FRAMES),
            "MIDI duration exceeds four hours"
        );
        Ok(frame as u64)
    }

    fn tempo(&mut self, tempo: u32) -> Result<()> {
        if self.metrical {
            ensure!(tempo != 0, "Invalid MIDI tempo");
            self.step = u128::from(tempo) * u128::from(RATE);
        }
        Ok(())
    }
}

pub(super) struct Sequence {
    events: Vec<(u64, Event)>,
    timeline: Timeline,
    pub(super) identity: SourceIdentity,
}

struct Timeline {
    end: u64,
    title: Option<String>,
}

struct MidiReader<'a, R> {
    inner: R,
    cancelled: Option<&'a AtomicBool>,
}

impl<R: Read> Read for MidiReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        check_cancelled(self.cancelled).map_err(std::io::Error::other)?;
        let len = buffer.len().min(64 * 1024);
        self.inner.read(&mut buffer[..len])
    }
}

impl Sequence {
    pub(super) fn read(file: RegularFile, cancelled: Option<&AtomicBool>) -> Result<Self> {
        let mut events = Vec::new();
        let (identity, timeline) = read_file(file, Some(&mut events), cancelled)?;
        Ok(Self {
            events,
            timeline,
            identity,
        })
    }

    pub(super) fn info(&self, path: &Path) -> MediaInfo {
        let mut info = self.timeline.info(path);
        info.channels = 2;
        info.sample_rate = RATE;
        info
    }
}

pub(super) fn probe(
    file: RegularFile,
    path: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<MediaInfo> {
    Ok(read_file(file, None, cancelled)?.1.info(path))
}

fn read_file(
    file: RegularFile,
    events: Option<&mut Vec<(u64, Event)>>,
    cancelled: Option<&AtomicBool>,
) -> Result<(SourceIdentity, Timeline)> {
    read_checked(
        &mut file.into_file(),
        events,
        cancelled,
        super::source_identity,
    )
}

// Both identity snapshots belong to the admitted descriptor, never its pathname.
fn read_checked<R: Read>(
    reader: &mut R,
    events: Option<&mut Vec<(u64, Event)>>,
    cancelled: Option<&AtomicBool>,
    identity: impl Fn(&R) -> Result<SourceIdentity>,
) -> Result<(SourceIdentity, Timeline)> {
    check_cancelled(cancelled)?;
    let before = identity(reader)?;
    let timeline = read_from(&mut *reader, events, cancelled)?;
    let after = identity(reader);
    check_cancelled(cancelled)?;
    ensure!(
        before == after?,
        anyhow::anyhow!("MIDI source changed while reading").context(super::SourceFailure)
    );
    Ok((before, timeline))
}

fn read_from(
    reader: impl Read,
    events: Option<&mut Vec<(u64, Event)>>,
    cancelled: Option<&AtomicBool>,
) -> Result<Timeline> {
    check_cancelled(cancelled)?;
    let mut bytes = Vec::new();
    let read = MidiReader {
        inner: reader,
        cancelled,
    }
    .take(MAX_BYTES + 1)
    .read_to_end(&mut bytes);
    check_cancelled(cancelled)?;
    read.map_err(super::source_io_error)?;
    let parsed = (|| {
        ensure!(bytes.len() as u64 <= MAX_BYTES, "MIDI file exceeds 8 MiB");
        Timeline::parse(&bytes, events, cancelled)
    })();
    check_cancelled(cancelled)?;
    parsed.context(super::SourceFailure)
}

impl Timeline {
    fn validate_headers(mut bytes: &[u8], cancelled: Option<&AtomicBool>) -> Result<()> {
        // Midly decodes a header before rejecting a duplicate. Validate all
        // chunk boundaries without touching track payloads before handing off.
        let mut seen = false;
        while !bytes.is_empty() {
            check_cancelled(cancelled)?;
            let (chunk, rest) = bytes
                .split_at_checked(8)
                .context("Truncated MIDI chunk header")?;
            let len = u32::from_be_bytes(chunk[4..].try_into()?) as usize;
            let (body, rest) = rest.split_at_checked(len).context("Truncated MIDI chunk")?;
            if &chunk[..4] == b"MThd" {
                ensure!(!seen, "Duplicate MIDI header");
                seen = true;
                let division = *body.get(4).context("Truncated MIDI header")?;
                ensure!(
                    division < 0x80 || matches!(division, 0xe8 | 0xe7 | 0xe3 | 0xe2),
                    "Invalid MIDI SMPTE frame rate"
                );
            }
            bytes = rest;
        }
        Ok(())
    }

    fn parse(
        bytes: &[u8],
        retained: Option<&mut Vec<(u64, Event)>>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        Self::validate_headers(bytes, cancelled)?;
        let (header, tracks) = midly::parse(bytes).context("Parsing standard MIDI file")?;
        ensure!(
            header.format != Format::Sequential,
            "MIDI format 2 is not supported"
        );
        let declared_tracks = tracks.size_hint().0;
        let retain_notes = retained.is_some();
        let mut tempos = Vec::new();
        let events = retained.unwrap_or(&mut tempos);
        let mut title = None;
        let mut last_tick = 0;
        let mut total = 0;
        let mut tracks_seen = 0;
        let mut used_port = None;
        for (index, track) in tracks.enumerate() {
            ensure!(index < 1024, "MIDI file exceeds 1024 tracks");
            tracks_seen += 1;
            let mut tick = 0;
            let mut ended = false;
            let mut port = 0;
            let mut track = track?;
            while let Some(event) = track.next() {
                total += 1;
                ensure!(total <= MAX_EVENTS, "MIDI file exceeds 500,000 events");
                if total % 1024 == 0 {
                    check_cancelled(cancelled)?;
                }
                let event = event?;
                tick += u64::from(event.delta.as_int());
                match event.kind {
                    TrackEventKind::Meta(MetaMessage::MidiPort(value)) => port = value.as_int(),
                    TrackEventKind::Midi { channel, message } => {
                        ensure!(
                            *used_port.get_or_insert(port) == port,
                            "MIDI files using multiple ports are not supported"
                        );
                        if retain_notes
                            && !matches!(
                                message,
                                MidiMessage::Aftertouch { .. }
                                    | MidiMessage::ChannelAftertouch { .. }
                            )
                        {
                            events.push((tick, Event::Midi(channel.as_int(), message)));
                        }
                    }
                    TrackEventKind::Meta(MetaMessage::Tempo(tempo))
                        if matches!(header.timing, Timing::Metrical(_)) =>
                    {
                        events.push((tick, Event::Tempo(tempo.as_int())));
                    }
                    TrackEventKind::Meta(MetaMessage::TrackName(name))
                        if index == 0 && title.is_none() =>
                    {
                        let name = String::from_utf8_lossy(&name[..name.len().min(1024)]);
                        let name = name.trim_matches(|c: char| c.is_whitespace() || c == '\0');
                        if !name.is_empty() {
                            title = Some(name.to_owned());
                        }
                    }
                    TrackEventKind::Meta(MetaMessage::EndOfTrack) => {
                        ensure!(track.next().is_none(), "MIDI data follows end-of-track");
                        ended = true;
                        break;
                    }
                    _ => (),
                }
            }
            ensure!(ended, "MIDI track has no end-of-track event");
            last_tick = last_tick.max(tick);
        }
        ensure!(
            tracks_seen > 0
                && tracks_seen == declared_tracks
                && (header.format != Format::SingleTrack || tracks_seen == 1),
            "Invalid MIDI track count"
        );
        // Stable order preserves file order for events sharing a tick.
        events.sort_by_key(|&(tick, _)| tick);
        let mut clock = Clock::new(header.timing)?;
        for (index, (at, event)) in events.iter_mut().enumerate() {
            if index % 1024 == 0 {
                check_cancelled(cancelled)?;
            }
            *at = clock.frame(*at)?;
            if let Event::Tempo(tempo) = *event {
                clock.tempo(tempo)?;
            }
        }
        if retain_notes {
            events.retain(|&(_, event)| matches!(event, Event::Midi(..)));
        }
        Ok(Self {
            end: clock.frame(last_tick)? + TAIL,
            title,
        })
    }

    fn info(&self, path: &Path) -> MediaInfo {
        let mut info = MediaInfo {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: Some(self.end as f64 / f64::from(RATE)),
            bitrate_bps: None,
            bitrate_exact: false,
            track_number: None,
            disc_number: None,
            bits_per_sample: None,
            release_date: None,
            replaygain_track_gain: None,
            codec: "MIDI".into(),
            channels: 0,
            sample_rate: 0,
        };
        info.apply_tags(
            rivu_metadata::Tags {
                title: self.title.clone(),
                ..Default::default()
            },
            path,
        );
        info
    }
}

pub(super) struct MidiSource {
    sequence: Sequence,
    bank: Arc<SoundFontLoad>,
    synth: Option<Synthesizer>,
    position: u64,
    next_event: usize,
    left: [f32; FRAGMENT],
    right: [f32; FRAGMENT],
}

pub(super) enum Step {
    Pcm(u64),
    Progress,
    Eof,
}

impl MidiSource {
    pub(super) fn new(sequence: Sequence) -> Self {
        Self::with_bank_load(sequence, Arc::clone(&BANK))
    }

    #[cfg(test)]
    pub(super) fn stalled_for_test(sequence: Sequence) -> (Self, crossbeam_channel::Sender<()>) {
        // Each call owns an independent load so tests can run in any order or
        // in parallel; a shared static would run the stalled loader only once,
        // leaving later callers with a dead release channel. Dropping
        // `release` (e.g. on test panic) disconnects `wait` and ends the loader.
        let (release, wait) = crossbeam_channel::bounded(1);
        let bank = Arc::new(SoundFontLoad::start(move || {
            let _ = wait.recv();
            anyhow::bail!("Test bank is unavailable")
        }));
        (Self::with_bank_load(sequence, bank), release)
    }

    fn with_bank_load(sequence: Sequence, bank: Arc<SoundFontLoad>) -> Self {
        Self {
            sequence,
            bank,
            synth: None,
            position: 0,
            next_event: 0,
            left: [0.; FRAGMENT],
            right: [0.; FRAGMENT],
        }
    }

    pub(super) fn opening_wait(&self) -> Option<&Receiver<()>> {
        self.synth.is_none().then_some(&self.bank.ready)
    }

    pub(super) fn poll_open(&mut self) -> Result<bool> {
        if self.synth.is_none() {
            let Some(bank) = self.bank.poll()? else {
                return Ok(false);
            };
            self.synth = Some(Self::synthesizer(bank)?);
        }
        Ok(true)
    }

    fn synthesizer(bank: &Arc<SoundFont>) -> Result<Synthesizer> {
        let mut settings = SynthesizerSettings::new(RATE as i32);
        settings.block_size = BLOCK;
        Ok(Synthesizer::new(bank, &settings)?)
    }

    pub(super) fn frames(&self) -> u64 {
        self.sequence.timeline.end
    }

    pub(super) fn prepare_seek(&mut self, frame: u64) -> Result<()> {
        if frame >= self.position {
            return Ok(());
        }
        // Recreate DSP history as well as MIDI voice/controller state.
        let bank = self.bank.poll()?.context("MIDI bank was not opened")?;
        self.synth = Some(Self::synthesizer(bank)?);
        self.position = 0;
        self.next_event = 0;
        Ok(())
    }

    pub(super) fn next(&mut self, pcm: &mut Vec<f32>, discard_before: u64) -> Step {
        if self.position >= self.sequence.timeline.end {
            return Step::Eof;
        }
        let synth = self.synth.as_mut().expect("MIDI source is open");
        for _ in 0..256 {
            let Some(&(frame, event)) = self.sequence.events.get(self.next_event) else {
                break;
            };
            if frame > self.position {
                break;
            }
            let Event::Midi(channel, message) = event else {
                unreachable!("validated sequences retain only channel messages")
            };
            Self::apply(synth, channel, message);
            self.next_event += 1;
        }
        let tail_start = self.sequence.timeline.end - TAIL;
        let next = self
            .sequence
            .events
            .get(self.next_event)
            .map_or(self.sequence.timeline.end, |event| event.0);
        let next = if self.position < tail_start {
            next.min(tail_start)
        } else {
            next
        };
        let start = self.position;
        let count = (next - start).min(FRAGMENT as u64) as usize;
        if count == 0 {
            return Step::Progress;
        }
        if start == tail_start {
            for channel in 0..16 {
                synth.process_midi_message(channel, 0xB0, 64, 0);
            }
            synth.note_off_all(false);
        }
        synth.render(&mut self.left[..count], &mut self.right[..count]);
        self.position += count as u64;
        if self.position <= discard_before {
            return Step::Progress;
        }
        let first = start.max(discard_before);
        pcm.clear();
        for i in (first - start) as usize..count {
            pcm.push(finite_sample(self.left[i]));
            pcm.push(finite_sample(self.right[i]));
        }
        Step::Pcm(first)
    }

    fn apply(synth: &mut Synthesizer, channel: u8, message: MidiMessage) {
        let (command, data1, data2) = match message {
            MidiMessage::NoteOff { key, vel } => (0x80, key.as_int(), vel.as_int()),
            MidiMessage::NoteOn { key, vel } => (0x90, key.as_int(), vel.as_int()),
            MidiMessage::Controller { controller, value } => {
                (0xB0, controller.as_int(), value.as_int())
            }
            MidiMessage::ProgramChange { program } => (0xC0, program.as_int(), 0),
            MidiMessage::PitchBend { bend } => {
                let raw = (bend.as_int() + 8192) as u16;
                (0xE0, (raw & 0x7f) as u8, (raw >> 7) as u8)
            }
            MidiMessage::Aftertouch { .. } | MidiMessage::ChannelAftertouch { .. } => return,
        };
        synth.process_midi_message(
            i32::from(channel),
            command,
            i32::from(data1),
            i32::from(data2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_read_cancels_between_short_reads() {
        struct ShortReads<'a> {
            cancelled: &'a AtomicBool,
            calls: usize,
        }

        impl Read for ShortReads<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                assert!(self.calls <= 2, "cancelled intake performed another read");
                assert!(buffer.len() <= 64 * 1024);
                buffer[0] = b'M';
                if self.calls == 2 {
                    self.cancelled
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(1)
            }
        }

        let cancelled = AtomicBool::new(false);
        let mut reader = ShortReads {
            cancelled: &cancelled,
            calls: 0,
        };
        let error = read_from(&mut reader, None, Some(&cancelled))
            .err()
            .unwrap();
        assert_eq!(reader.calls, 2);
        assert!(error.to_string().contains("cancelled"));
        assert!(!crate::source::is_source_failure(&error));
    }

    #[test]
    fn midi_read_preserves_the_byte_limit_and_io_failure_provenance() {
        let mut reader = std::io::repeat(0).take(MAX_BYTES + 2);
        let error = read_from(&mut reader, None, None).err().unwrap();
        assert_eq!(reader.limit(), 1);
        assert!(format!("{error:#}").contains("exceeds 8 MiB"));
        assert!(crate::source::is_source_failure(&error));

        struct FailedRead(i32);
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(self.0))
            }
        }

        for (code, source_failure) in [(libc::EIO, true), (libc::EMFILE, false)] {
            let error = read_from(FailedRead(code), None, None).err().unwrap();
            assert_eq!(crate::source::is_source_failure(&error), source_failure);
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(code)
            );
        }
    }

    #[test]
    fn validates_generator_boundaries_and_record_shape() {
        use super::fixture::chunk;
        for target in [b"pgen", b"igen"] {
            for (size, boundary, valid) in [
                (Some(8), 1_u16, true), // One generator and its terminator.
                (Some(8), 2, false),    // Would include the terminator in the zone.
                (Some(4), 0, true),     // A terminator and an empty zone.
                (Some(6), 0, false),    // A partial generator record.
                (Some(0), 0, false),    // No terminator.
                (None, 0, false),       // Missing table.
            ] {
                let mut pdta = b"pdta".to_vec();
                for (header, record, offset, bag, generator) in [
                    (b"phdr", 38, 24, b"pbag", b"pgen"),
                    (b"inst", 22, 20, b"ibag", b"igen"),
                ] {
                    let mut headers = vec![0; 2 * record];
                    headers[record + offset] = 1; // Terminal header points at the terminal bag.
                    chunk(&mut pdta, header, &headers);

                    let (size, boundary) = if generator == target {
                        (size, boundary)
                    } else {
                        (Some(8), 1)
                    };
                    let mut bags = [0; 8];
                    bags[4..6].copy_from_slice(&boundary.to_le_bytes());
                    chunk(&mut pdta, bag, &bags);
                    if let Some(size) = size {
                        chunk(&mut pdta, generator, &vec![0; size]);
                    }
                }

                let mut riff = b"sfbk".to_vec();
                chunk(&mut riff, b"LIST", b"INFO");
                chunk(&mut riff, b"LIST", b"sdta");
                chunk(&mut riff, b"LIST", &pdta);
                let mut bank = Vec::new();
                chunk(&mut bank, b"RIFF", &riff);
                let len = bank.len() as u64;
                let result = validate_bank_chunks(&mut std::io::Cursor::new(bank), len);
                assert_eq!(
                    result.is_ok(),
                    valid,
                    "{} size {size:?}, boundary {boundary}: {result:?}",
                    std::str::from_utf8(target).unwrap()
                );
            }
        }
    }

    #[test]
    fn mixed_track_tempo_and_end_ticks_share_one_clock() {
        // 480 ticks at the default tempo, then 480 at one second per beat.
        let bytes = b"MThd\0\0\0\x06\0\x01\0\x02\x01\xe0\
            MTrk\0\0\0\x0c\x83\x60\xff\x51\x03\x0f\x42\x40\0\xff\x2f\0\
            MTrk\0\0\0\x0d\0\x90\x3c\x64\x87\x40\x80\x3c\0\0\xff\x2f\0";
        let mut events = Vec::new();
        let timeline = Timeline::parse(bytes, Some(&mut events), None).unwrap();
        assert_eq!(events[0].0, 0);
        assert_eq!(events[1].0, 72_000);
        assert_eq!(timeline.end, 120_000);
        let scanned = Timeline::parse(bytes, None, None).unwrap();
        assert_eq!(scanned.end, timeline.end);
    }

    #[test]
    fn rejects_missing_tracks_and_events_after_end() {
        let missing = b"MThd\0\0\0\x06\0\x01\0\x02\x01\xe0MTrk\0\0\0\x04\0\xff\x2f\0";
        let extra = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk\0\0\0\x08\0\xff\x2f\0\0\x90\x3c\x64";
        for bytes in [&missing[..], &extra[..]] {
            assert!(Timeline::parse(bytes, None, None).is_err());
        }
    }

    #[test]
    fn rejects_invalid_smpte_timing_in_primary_and_duplicate_headers() {
        let bad_header = b"MThd\0\0\0\x06\0\0\0\x01\x80\x64";
        let track = b"MTrk\0\0\0\x04\0\xff\x2f\0";
        let invalid = [&bad_header[..], &track[..]].concat();
        let duplicate = [
            &b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0"[..],
            &track[..],
            &bad_header[..],
        ]
        .concat();
        for (bytes, message) in [(invalid, "SMPTE"), (duplicate, "Duplicate")] {
            let error = Timeline::parse(&bytes, None, None).err().unwrap();
            assert!(error.to_string().contains(message));
        }
    }

    #[test]
    fn rejects_multiple_used_ports_for_probe_and_render() {
        let mut bytes = *b"MThd\0\0\0\x06\0\x01\0\x02\x01\xe0\
            MTrk\0\0\0\x08\0\x90\x3c\x64\0\xff\x2f\0\
            MTrk\0\0\0\x0d\0\xff\x21\x01\x01\0\x90\x3c\x64\0\xff\x2f\0";
        for retain in [false, true] {
            let mut events = Vec::new();
            let error = Timeline::parse(&bytes, retain.then_some(&mut events), None)
                .err()
                .unwrap();
            assert!(error.to_string().contains("multiple ports"));
        }
        bytes[42] = 0; // Explicit port zero and the implicit default are identical.
        assert!(Timeline::parse(&bytes, Some(&mut Vec::new()), None).is_ok());
    }

    #[test]
    fn drop_frame_clock_ignores_tempo_and_uses_the_exact_rate() {
        let mut clock = Clock::new(Timing::Timecode(Fps::Fps29, 100)).unwrap();
        clock.tempo(1_000_000).unwrap();
        assert_eq!(clock.frame(300_000).unwrap(), 4_804_800);
    }
}
