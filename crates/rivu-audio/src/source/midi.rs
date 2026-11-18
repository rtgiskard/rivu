//! Standard MIDI file timing and bounded SoundFont synthesis.

use super::{check_cancelled, finite_sample};
use crate::MediaInfo;
use anyhow::{Context, Result, ensure};
use crossbeam_channel::{Receiver, TryRecvError};
use midly::{Format, Fps, MetaMessage, MidiMessage, Timing, TrackEventKind};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, OnceLock, atomic::AtomicBool},
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
    pub(super) end: u64,
    title: Option<String>,
}

impl Sequence {
    pub(super) fn read(
        file: File,
        retain_notes: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(super::source_io_error)?;
        let parsed = (|| {
            ensure!(bytes.len() as u64 <= MAX_BYTES, "MIDI file exceeds 8 MiB");
            Self::parse(&bytes, retain_notes, cancelled)
        })();
        check_cancelled(cancelled)?;
        parsed.context(super::SourceFailure)
    }

    fn parse(bytes: &[u8], retain_notes: bool, cancelled: Option<&AtomicBool>) -> Result<Self> {
        let (header, tracks) = midly::parse(bytes).context("Parsing standard MIDI file")?;
        ensure!(
            header.format != Format::Sequential,
            "MIDI format 2 is not supported"
        );
        let declared_tracks = tracks.size_hint().0;
        let mut events = Vec::new();
        let mut title = None;
        let mut last_tick = 0;
        let mut total = 0;
        let mut tracks_seen = 0;
        for (index, track) in tracks.enumerate() {
            ensure!(index < 1024, "MIDI file exceeds 1024 tracks");
            tracks_seen += 1;
            let mut tick = 0;
            let mut ended = false;
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
                    TrackEventKind::Midi { channel, message }
                        if retain_notes
                            && !matches!(
                                message,
                                MidiMessage::Aftertouch { .. }
                                    | MidiMessage::ChannelAftertouch { .. }
                            ) =>
                    {
                        events.push((tick, Event::Midi(channel.as_int(), message)));
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
        events.retain(|&(_, event)| matches!(event, Event::Midi(..)));
        Ok(Self {
            events,
            end: clock.frame(last_tick)? + TAIL,
            title,
        })
    }

    pub(super) fn info(&self, path: &Path, rendering: bool) -> MediaInfo {
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
            channels: if rendering { 2 } else { 0 },
            sample_rate: if rendering { RATE } else { 0 },
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

type SoundFontResult = OnceLock<Result<Arc<SoundFont>, String>>;

struct SoundFontLoad {
    result: Arc<SoundFontResult>,
    ready: Receiver<()>,
}

impl SoundFontLoad {
    fn start(load: impl FnOnce() -> Result<Arc<SoundFont>> + Send + 'static) -> Self {
        let result = Arc::new(SoundFontResult::new());
        let published = Arc::clone(&result);
        let (done, ready) = crossbeam_channel::bounded(0);
        let started = std::thread::Builder::new()
            .name("rivu-midi-bank".into())
            .spawn(move || {
                let _ = published.set(load().map_err(|error| format!("{error:#}")));
                // Disconnection is a one-shot broadcast, not a consumable wake.
                drop(done);
            });
        if let Err(error) = started {
            let _ = result.set(Err(format!("Starting MIDI SoundFont loader: {error}")));
        }
        Self { result, ready }
    }

    fn poll(&self) -> Result<Option<&Arc<SoundFont>>> {
        if self.ready.try_recv() == Err(TryRecvError::Empty) {
            return Ok(None);
        }
        self.result
            .get()
            .context("MIDI SoundFont loader stopped without a result")?
            .as_ref()
            .map(Some)
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

// One load and one cached result per process, including failures. Dropping a
// source abandons its wait, not the process-wide bank needed by other sources.
static BANK: LazyLock<Arc<SoundFontLoad>> =
    LazyLock::new(|| Arc::new(SoundFontLoad::start(load_soundfont)));

fn load_soundfont() -> Result<Arc<SoundFont>> {
    let path = std::env::var_os("RIVU_MIDI_SOUNDFONT").map_or_else(
        || {
            [
                "/usr/share/soundfonts/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/FluidR3_GM.sf2",
                "/usr/share/sounds/sf2/default-GM.sf2",
            ]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .map(PathBuf::from)
        },
        |path| Some(PathBuf::from(path)),
    ).context("No General MIDI SoundFont found; install a GM .sf2 or set RIVU_MIDI_SOUNDFONT and restart Rivu")?;
    let file = rivu_utils::regular_file::open_regular_file(&path)
        .with_context(|| format!("Opening MIDI SoundFont {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    validate_bank_chunks(&mut reader, len)
        .with_context(|| format!("Validating MIDI SoundFont {}", path.display()))?;
    reader.rewind()?;
    let bank = SoundFont::new(&mut reader)
        .with_context(|| format!("Loading MIDI SoundFont {}", path.display()))?;
    Ok(Arc::new(bank))
}

fn validate_bank_chunks(reader: &mut (impl Read + Seek), len: u64) -> Result<()> {
    ensure!(len <= 512 * 1024 * 1024, "SoundFont exceeds 512 MiB");
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    let end = 8 + u64::from(u32::from_le_bytes(header[4..8].try_into()?));
    ensure!(
        &header[..4] == b"RIFF" && &header[8..] == b"sfbk" && end <= len,
        "Invalid SoundFont RIFF header"
    );
    let mut cursor = 12;
    let mut tables = [(0, 0); 6];
    // Mirror the parser's list boundaries without reading or allocating samples.
    for kind in [b"INFO", b"sdta", b"pdta"] {
        ensure!(cursor + 12 <= end, "Truncated SoundFont list header");
        reader.read_exact(&mut header)?;
        let list_end = cursor + 8 + u64::from(u32::from_le_bytes(header[4..8].try_into()?));
        cursor += 12;
        ensure!(
            &header[..4] == b"LIST" && &header[8..] == kind && (cursor..=end).contains(&list_end),
            "Invalid SoundFont list"
        );
        if kind != b"sdta" {
            ensure!(
                list_end - cursor <= 4 * 1024 * 1024,
                "SoundFont metadata list exceeds 4 MiB"
            );
        }
        while cursor < list_end {
            ensure!(cursor + 8 <= list_end, "Truncated SoundFont chunk header");
            reader.read_exact(&mut header[..8])?;
            let size = u64::from(u32::from_le_bytes(header[4..8].try_into()?));
            let id = &header[..4];
            if kind == b"INFO" && matches!(id, b"ifil" | b"iver") {
                ensure!(size == 4, "Invalid SoundFont version chunk size");
            }
            if let Some(index) = [b"phdr", b"pbag", b"pgen", b"inst", b"ibag", b"igen"]
                .iter()
                .position(|tag| *tag == id)
            {
                ensure!(tables[index] == (0, 0), "Duplicate SoundFont table");
                tables[index] = (cursor + 8, size);
            }
            cursor += 8 + size;
            ensure!(cursor <= list_end, "SoundFont chunk extends past its list");
            reader.seek(SeekFrom::Start(cursor))?;
        }
    }
    for (set, record, zone_offset) in [(0, 38, 24), (3, 22, 20)] {
        let [headers, bags, (_, generator_bytes)] = tables[set..set + 3].try_into()?;
        validate_table_indices(reader, headers, record, zone_offset, bags.1 / 4)?;
        ensure!(
            generator_bytes != 0 && generator_bytes.is_multiple_of(4),
            "Invalid SoundFont generator table size"
        );
        // The last generator is a terminator, so the largest legal exclusive
        // boundary is the number of records minus one.
        validate_table_indices(reader, bags, 4, 0, generator_bytes / 4)?;
    }
    Ok(())
}

// Header records point into bags; bags point into the generator table. The
// dependency builds ranges from adjacent records, with no bounds checks.
fn validate_table_indices(
    reader: &mut (impl Read + Seek),
    (offset, size): (u64, u64),
    record: usize,
    index_offset: usize,
    limit: u64,
) -> Result<()> {
    ensure!(
        size.is_multiple_of(record as u64) && size / record as u64 <= 65_536,
        "Invalid SoundFont table size"
    );
    reader.seek(SeekFrom::Start(offset))?;
    let mut row = [0; 38];
    let mut previous = 0;
    for _ in 0..size / record as u64 {
        reader.read_exact(&mut row[..record])?;
        let index = u64::from(u16::from_le_bytes(
            row[index_offset..index_offset + 2].try_into()?,
        ));
        ensure!(
            (previous..limit).contains(&index),
            "Invalid SoundFont table index"
        );
        previous = index;
    }
    Ok(())
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
            let _ = wait.recv_timeout(std::time::Duration::from_secs(10));
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
        self.sequence.end
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
        if self.position >= self.sequence.end {
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
                unreachable!()
            };
            Self::apply(synth, channel, message);
            self.next_event += 1;
        }
        let tail_start = self.sequence.end - TAIL;
        let next = self
            .sequence
            .events
            .get(self.next_event)
            .map_or(self.sequence.end, |event| event.0);
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
    fn validates_generator_boundaries_and_record_shape() {
        fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]) {
            out.extend_from_slice(tag);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
        }

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
        let sequence = Sequence::parse(bytes, true, None).unwrap();
        assert_eq!(sequence.events[0].0, 0);
        assert_eq!(sequence.events[1].0, 72_000);
        assert_eq!(sequence.end, 120_000);
        let scanned = Sequence::parse(bytes, false, None).unwrap();
        assert_eq!(scanned.end, sequence.end);
        assert!(scanned.events.is_empty());
    }

    #[test]
    fn rejects_missing_tracks_and_events_after_end() {
        let missing = b"MThd\0\0\0\x06\0\x01\0\x02\x01\xe0MTrk\0\0\0\x04\0\xff\x2f\0";
        let extra = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk\0\0\0\x08\0\xff\x2f\0\0\x90\x3c\x64";
        for bytes in [&missing[..], &extra[..]] {
            assert!(Sequence::parse(bytes, false, None).is_err());
        }
    }

    #[test]
    fn drop_frame_clock_ignores_tempo_and_uses_the_exact_rate() {
        let mut clock = Clock::new(Timing::Timecode(Fps::Fps29, 100)).unwrap();
        clock.tempo(1_000_000).unwrap();
        assert_eq!(clock.frame(300_000).unwrap(), 4_804_800);
    }
}
