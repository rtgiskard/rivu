use anyhow::{Context, Result, bail};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

/// An audio CUE sheet. INDEX 01 starts a track; the next INDEX 01 in the same
/// FILE ends it. INDEX 00 pregaps therefore remain at the end of the prior track.
#[derive(Debug)]
pub struct CueSheet {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub tracks: Vec<CueTrack>,
    pub(super) retained: Option<super::input::RetainedScope>,
}

#[derive(Debug)]
pub struct CueTrack {
    pub number: u32,
    pub file: PathBuf,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
}

struct PendingTrack {
    number: u32,
    file: PathBuf,
    file_group: usize,
    title: Option<String>,
    performer: Option<String>,
    index_zero: Option<u64>,
    start: Option<u64>,
}

/// Parse already bounded input without reopening its sheet.
pub(super) fn parse_input(
    bytes: &[u8],
    path: &Path,
    cancellation: &super::ScanCancellation,
) -> Result<CueSheet> {
    cancellation.check()?;
    let text = decode(bytes).with_context(|| format!("Decoding CUE {}", path.display()))?;
    cancellation.check()?;
    parse_checked(
        &text,
        path.parent().unwrap_or_else(|| Path::new(".")),
        || cancellation.check(),
        None,
        0,
    )
    .with_context(|| format!("Parsing CUE {}", path.display()))
}

pub(super) fn parse_import_input(
    bytes: &[u8],
    path: &Path,
    cancellation: &super::ScanCancellation,
    budget: &super::input::RetainedBudget,
) -> Result<CueSheet> {
    cancellation.check()?;
    let text = decode(bytes).with_context(|| format!("Decoding CUE {}", path.display()))?;
    parse_checked(
        &text,
        path.parent().unwrap_or_else(|| Path::new(".")),
        || cancellation.check(),
        Some(budget),
        path.as_os_str().len() + std::mem::size_of::<PathBuf>(),
    )
    .with_context(|| format!("Parsing CUE {}", path.display()))
}

fn decode(bytes: &[u8]) -> Result<Cow<'_, str>> {
    let (little_endian, data) = if let Some(data) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (true, data)
    } else if let Some(data) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (false, data)
    } else {
        return std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map(Cow::Borrowed)
            .context(
                "CUE must be UTF-8 or BOM-marked UTF-16; convert legacy encodings before import",
            );
    };
    if data.len() % 2 != 0 {
        bail!("Incomplete UTF-16 CUE character");
    }
    let (pairs, remainder) = data.as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    let words: Vec<u16> = pairs
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16(&words)
        .map(Cow::Owned)
        .context("Invalid UTF-16 CUE")
}

fn split_word(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map_or((line, ""), |(word, rest)| (word, rest.trim()))
}

fn string_value(value: &str) -> Result<&str> {
    if let Some(quoted) = value.strip_prefix('"') {
        let Some((text, rest)) = quoted.split_once('"') else {
            bail!("Unclosed quoted value");
        };
        if !rest.trim().is_empty() {
            bail!("Unexpected text after quoted value");
        }
        Ok(text)
    } else {
        Ok(value)
    }
}

fn timestamp(value: &str) -> Result<u64> {
    let mut parts = value.split(':');
    let minute: u64 = parts
        .next()
        .context("Missing CUE minute")?
        .parse()
        .context("Invalid CUE minute")?;
    let second: u64 = parts
        .next()
        .context("Missing CUE second")?
        .parse()
        .context("Invalid CUE second")?;
    let frame: u64 = parts
        .next()
        .context("Missing CUE frame")?
        .parse()
        .context("Invalid CUE frame")?;
    if parts.next().is_some() {
        bail!("Expected CUE time MM:SS:FF, got {value}");
    }
    if second >= 60 || frame >= 75 {
        bail!("CUE seconds must be below 60 and frames below 75");
    }
    minute
        .checked_mul(60)
        .and_then(|v| v.checked_add(second))
        .and_then(|v| v.checked_mul(75))
        .and_then(|v| v.checked_add(frame))
        .context("CUE time is too large")
}

#[cfg(test)]
fn parse(text: &str, parent: &Path) -> Result<CueSheet> {
    parse_checked(text, parent, || Ok(()), None, 0)
}

fn parse_checked(
    text: &str,
    parent: &Path,
    mut check: impl FnMut() -> Result<()>,
    budget: Option<&super::input::RetainedBudget>,
    sheet_path_bytes: usize,
) -> Result<CueSheet> {
    let mut retained = budget.map(super::input::RetainedBudget::scoped);
    if let Some(retained) = &mut retained {
        retained.reserve(std::mem::size_of::<CueSheet>() + sheet_path_bytes)?;
    }
    let mut workspace = budget.map(super::input::RetainedBudget::scoped);
    let mut file_bytes = 0;
    let mut sheet = CueSheet {
        title: None,
        performer: None,
        tracks: Vec::new(),
        retained: None,
    };
    let mut pending: Vec<PendingTrack> = Vec::new();
    let mut file = None;
    let mut file_group = 0;
    for (line_number, raw) in text.lines().enumerate() {
        check()?;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) = split_word(line);
        let result: Result<()> = (|| {
            match word {
                word if word.eq_ignore_ascii_case("FILE") => {
                    let (name, kind) = if let Some(quoted) = rest.strip_prefix('"') {
                        let (name, kind) = quoted.split_once('"').context("Unclosed FILE name")?;
                        (name, kind.trim())
                    } else {
                        split_word(rest)
                    };
                    let next_file_bytes = name.len()
                        + if Path::new(name).is_absolute() {
                            0
                        } else {
                            parent.as_os_str().len() + 1
                        };
                    if let Some(workspace) = &mut workspace {
                        workspace.reserve(next_file_bytes)?;
                    }
                    if name.is_empty() || kind.is_empty() || kind.split_whitespace().count() != 1 {
                        bail!("FILE requires a filename and a file type");
                    }
                    // CUE files commonly use Windows path separators even on Unix.
                    let name = PathBuf::from(name.replace('\\', "/"));
                    file = Some(if name.is_absolute() {
                        name
                    } else {
                        parent.join(name)
                    });
                    if let Some(workspace) = &mut workspace {
                        workspace.release(file_bytes);
                    }
                    file_bytes = next_file_bytes;
                    file_group += 1;
                }
                word if word.eq_ignore_ascii_case("TRACK") => {
                    let (number, kind) = split_word(rest);
                    let number: u32 = number.parse().context("Invalid TRACK number")?;
                    if !(1..=99).contains(&number)
                        || pending.last().is_some_and(|p| p.number >= number)
                    {
                        bail!("TRACK numbers must increase and be in 01..99");
                    }
                    if !kind.eq_ignore_ascii_case("AUDIO") {
                        bail!("Only AUDIO CUE tracks are supported");
                    }
                    if let Some(retained) = &mut retained {
                        retained.reserve(file.as_ref().map_or(0, |path| path.as_os_str().len()))?;
                    }
                    if let Some(workspace) = &mut workspace {
                        workspace.reserve_vec(&mut pending, 1)?;
                    }
                    pending.push(PendingTrack {
                        number,
                        file: file.clone().context("TRACK requires a preceding FILE")?,
                        file_group,
                        title: None,
                        performer: None,
                        index_zero: None,
                        start: None,
                    });
                }
                word if word.eq_ignore_ascii_case("TITLE")
                    || word.eq_ignore_ascii_case("PERFORMER") =>
                {
                    let value = string_value(rest)?;
                    if let Some(retained) = &mut retained {
                        retained.reserve(value.len())?;
                    }
                    let title = word.eq_ignore_ascii_case("TITLE");
                    let slot = if let Some(track) = pending.last_mut() {
                        if title {
                            &mut track.title
                        } else {
                            &mut track.performer
                        }
                    } else if title {
                        &mut sheet.title
                    } else {
                        &mut sheet.performer
                    };
                    let old = slot.replace(value.to_owned());
                    if let (Some(retained), Some(old)) = (&mut retained, old) {
                        retained.release(old.len());
                    }
                }
                word if word.eq_ignore_ascii_case("INDEX") => {
                    let (index, time) = split_word(rest);
                    let index: u32 = index.parse().context("Invalid INDEX number")?;
                    if index > 99 {
                        bail!("INDEX number must be in 00..99");
                    }
                    let time = timestamp(time)?;
                    let track = pending.last_mut().context("INDEX requires a TRACK")?;
                    if track.file_group != file_group {
                        bail!("INDEX requires a TRACK in the current FILE");
                    }
                    match index {
                        0 => {
                            if track.index_zero.replace(time).is_some() {
                                bail!("Duplicate INDEX 00");
                            }
                        }
                        1 if track.start.replace(time).is_some() => {
                            bail!("Duplicate INDEX 01");
                        }
                        _ => {}
                    }
                }
                word if word.eq_ignore_ascii_case("PREGAP")
                    || word.eq_ignore_ascii_case("POSTGAP") =>
                {
                    if pending.is_empty() {
                        bail!("Gap requires a TRACK");
                    }
                    timestamp(rest)?;
                    // These describe generated CD silence, not samples in FILE.
                }
                word if [
                    "REM",
                    "CATALOG",
                    "CDTEXTFILE",
                    "SONGWRITER",
                    "ISRC",
                    "FLAGS",
                ]
                .iter()
                .any(|directive| word.eq_ignore_ascii_case(directive)) => {}
                _ => bail!("Unsupported CUE directive {word}"),
            }
            Ok(())
        })();
        result.with_context(|| format!("line {}", line_number + 1))?;
    }
    if pending.is_empty() {
        bail!("CUE contains no audio tracks");
    }
    let mut pending = pending.into_iter().peekable();
    while let Some(track) = pending.next() {
        check()?;
        let start = track
            .start
            .with_context(|| format!("TRACK {:02} has no INDEX 01", track.number))?;
        if track.index_zero.is_some_and(|zero| zero > start) {
            bail!("TRACK {:02}: INDEX 00 is after INDEX 01", track.number);
        }
        let end = pending
            .peek()
            .filter(|next| next.file_group == track.file_group)
            .map(|next| next.start.context("Next track has no INDEX 01"))
            .transpose()?;
        if end.is_some_and(|end| end <= start) {
            bail!("Track times must increase within each FILE");
        }
        if let Some(retained) = &mut retained {
            retained.reserve_vec(&mut sheet.tracks, 1)?;
        }
        sheet.tracks.push(CueTrack {
            number: track.number,
            file: track.file,
            title: track.title,
            performer: track.performer,
            start_frame: start,
            end_frame: end,
        });
    }
    sheet.retained = retained;
    Ok(sheet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_files_metadata_and_cd_frame_boundaries() {
        let sheet = parse(
            r#"PERFORMER "专辑歌手"
TITLE "Album"
FILE "disc one.flac" WAVE
  TRACK 01 AUDIO
    TITLE "First"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Second"
    PERFORMER "Guest"
    INDEX 00 01:01:00
    INDEX 01 01:02:37
FILE "disc two.wav" WAVE
  TRACK 03 AUDIO
    INDEX 01 00:00:00
"#,
            Path::new("/music"),
        )
        .unwrap();
        assert_eq!(sheet.title.as_deref(), Some("Album"));
        assert_eq!(sheet.performer.as_deref(), Some("专辑歌手"));
        assert_eq!(sheet.tracks[0].file, Path::new("/music/disc one.flac"));
        assert_eq!(sheet.tracks[0].end_frame, Some(62 * 75 + 37));
        assert_eq!(sheet.tracks[1].start_frame, 62 * 75 + 37);
        assert_eq!(sheet.tracks[1].end_frame, None);
        assert_eq!(sheet.tracks[1].performer.as_deref(), Some("Guest"));
        assert_eq!(sheet.tracks[2].start_frame, 0);
        assert_eq!(sheet.tracks[2].file, Path::new("/music/disc two.wav"));
    }

    #[test]
    fn rejects_ambiguous_or_invalid_boundaries() {
        for text in [
            "FILE a.wav WAVE\nTRACK 01 AUDIO",
            "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:75",
            "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:60:00",
            "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:02:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00",
            "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nINDEX 01 00:01:00",
            "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 00 00:02:00\nINDEX 01 00:01:00",
            "TRACK 01 AUDIO\nINDEX 01 00:00:00",
            "FILE a.bin BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00",
        ] {
            assert!(parse(text, Path::new(".")).is_err(), "{text}");
        }
    }

    #[test]
    fn reads_utf8_and_bom_marked_utf16_without_loss() {
        let text = "TITLE \"中文\"\n";
        assert_eq!(decode(text.as_bytes()).unwrap(), text);
        for little in [true, false] {
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for word in text.encode_utf16() {
                bytes.extend(if little {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                });
            }
            assert_eq!(decode(&bytes).unwrap(), text);
        }
        assert!(decode(&[0xff, 0xfe, 1]).is_err());
        assert!(decode(&[0xff, 0xfe, 0x00, 0xd8]).is_err());
    }
    #[test]
    fn parsing_stops_at_line_and_segment_boundaries_when_cancelled() {
        for cancel_at in [2, 6, 7] {
            let cancellation = crate::library::ScanCancellation::default();
            let mut checks = 0;
            let result = parse_checked(
                "FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n",
                Path::new("."),
                || {
                    checks += 1;
                    if checks == cancel_at {
                        cancellation.cancel();
                    }
                    cancellation.check()
                },
                None,
                0,
            );
            assert!(result.is_err());
            assert_eq!(checks, cancel_at);
        }
    }

    #[test]
    fn parsed_sheet_budget_tracks_live_metadata_not_replaced_titles_or_workspace() {
        let budget = crate::library::input::RetainedBudget::default();
        budget
            .reserve(crate::library::input::MAX_IMPORT_RETAINED_BYTES - 8192)
            .unwrap();
        let mut text = String::new();
        for _ in 0..99 {
            text.push_str(&format!("TITLE \"{}\"\n", "x".repeat(1024)));
        }
        text.push_str("FILE album.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        let sheet = parse_import_input(
            text.as_bytes(),
            Path::new("/music/album.cue"),
            &crate::library::ScanCancellation::default(),
            &budget,
        )
        .unwrap();
        assert_eq!(sheet.title.as_ref().unwrap().len(), 1024);
        assert!(budget.remaining() > 5000);
        drop(sheet);
        assert_eq!(budget.remaining(), 8192);
    }
}
