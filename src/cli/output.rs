use anyhow::{Context, Result, bail};
use rivu::i18n::Localizer;
use rivu_protocol::{
    model::{MAX_QUERY_ROWS, PlaybackStatus, Query},
    response::{QueryError, StateResponse, StateSections, ViewResponse},
};
use std::{
    io::{self, Write},
    path::Path,
};

// Escape only untrusted display values, never stored data or layout separators.
pub(super) struct Terminal<T>(pub(super) T);

impl<T: std::fmt::Display> std::fmt::Display for Terminal<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Escaped<'a, 'b>(&'a mut std::fmt::Formatter<'b>);

        impl std::fmt::Write for Escaped<'_, '_> {
            fn write_str(&mut self, value: &str) -> std::fmt::Result {
                let mut start = 0;
                for (index, character) in value.char_indices() {
                    if !character.is_control() {
                        continue;
                    }
                    self.0.write_str(&value[start..index])?;
                    match character {
                        '\n' => self.0.write_str("\\n")?,
                        '\r' => self.0.write_str("\\r")?,
                        '\t' => self.0.write_str("\\t")?,
                        '\0' => self.0.write_str("\\0")?,
                        '\u{1}'..='\u{7f}' => write!(self.0, "\\x{:02x}", character as u32)?,
                        _ => write!(self.0, "\\u{{{:04x}}}", character as u32)?,
                    }
                    start = index + character.len_utf8();
                }
                self.0.write_str(&value[start..])
            }
        }

        use std::fmt::Write as _;
        let alternate = formatter.alternate();
        let mut output = Escaped(formatter);
        if alternate {
            write!(output, "{:#}", self.0)
        } else {
            write!(output, "{}", self.0)
        }
    }
}

pub(super) fn terminal_text(value: &str) -> std::borrow::Cow<'_, str> {
    if value.chars().any(char::is_control) {
        Terminal(value).to_string().into()
    } else {
        value.into()
    }
}

pub(super) fn list_queue(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let state = session.get_state(StateSections {
        queue: true,
        ..StateSections::default()
    })?;
    let current = state
        .queue
        .context("State omitted queue section")?
        .current_id;
    let mut after = None;
    let mut expected = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut first = true;
    if json {
        write!(output, "[")?;
    }
    loop {
        let response = session.query(Query::QueueBatch {
            query: None,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = listing_result(i18n, response.result)?;
        let ViewResponse::QueueBatch(batch) = view else {
            bail!("Queue query returned an unexpected response");
        };
        for row in &batch.rows {
            if json {
                if !first {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, row)?;
                first = false;
            } else {
                writeln!(
                    output,
                    "{}\t{}\t{}\t{}{}",
                    row.position,
                    row.id,
                    row.track_id,
                    Terminal(&row.track.title),
                    if current == Some(row.id) { " *" } else { "" }
                )?;
            }
        }
        let Some(next) = batch.next else {
            break;
        };
        if batch.rows.is_empty() || after == Some(next) {
            bail!("Queue query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

pub(super) fn list_library(
    i18n: &Localizer,
    data_dir: &Path,
    json: bool,
    query: Option<String>,
    favorite: Option<bool>,
    missing: Option<bool>,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let _ = session.get_state(StateSections::default())?;
    let mut after = None;
    let mut expected = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        write!(output, "[")?;
    }
    let mut first = true;
    loop {
        let response = session.query(Query::TrackBatch {
            query: query.clone(),
            favorite,
            missing,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = listing_result(i18n, response.result)?;
        let ViewResponse::TrackBatch(batch) = view else {
            bail!("Library query returned an unexpected response");
        };
        for track in &batch.rows {
            if json {
                if !first {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, track)?;
                first = false;
            } else {
                write!(
                    output,
                    "{}\t{}\t{}\t{}\t{}{}{}\t",
                    track.id,
                    Terminal(&track.title),
                    Terminal(&track.artist),
                    Terminal(&track.album),
                    Terminal(track.path.display()),
                    if track.missing {
                        i18n.text("core-missing-marker")
                    } else {
                        ""
                    },
                    if track.favorite {
                        i18n.text("core-favorite-marker")
                    } else {
                        ""
                    }
                )?;
                match track.bitrate_bps {
                    Some(value) => writeln!(
                        output,
                        "{}{:.1} kbps",
                        if track.bitrate_exact { "" } else { "≈" },
                        value as f64 / 1000.0
                    )?,
                    None => writeln!(output, "—")?,
                }
            }
        }
        if batch.rows.is_empty() && batch.next.is_some() {
            bail!("Library query returned an empty batch with a continuation cursor");
        }
        let Some(next) = batch.next else {
            break;
        };
        if after == Some(next) {
            bail!("Library query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

pub(super) fn show_other_tracks(i18n: &Localizer, view: ViewResponse, json: bool) -> Result<()> {
    match view {
        ViewResponse::LibraryPage(page) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                println!("total\t{}", page.total);
                for track in page.rows {
                    println!(
                        "{}\t{}\t{}\t{}\t{}{}{}",
                        track.id,
                        Terminal(&track.title),
                        Terminal(&track.artist),
                        Terminal(&track.album),
                        Terminal(track.path.display()),
                        if track.missing {
                            i18n.text("core-missing-marker")
                        } else {
                            ""
                        },
                        if track.favorite {
                            i18n.text("core-favorite-marker")
                        } else {
                            ""
                        },
                    );
                }
            }
        }
        ViewResponse::OtherTracksCount(count) => println!("{count}"),
        ViewResponse::LibraryLocate(location) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&location)?);
            } else {
                use rivu_protocol::model::LibraryLocateResult;
                match location {
                    LibraryLocateResult::Located(location) => {
                        println!("located\t{}\t{}", location.offset, location.total);
                    }
                    LibraryLocateResult::Filtered => println!("filtered"),
                    LibraryLocateResult::Unavailable => println!("unavailable"),
                    LibraryLocateResult::OutsideRoot => println!("outside-root"),
                }
            }
        }
        _ => bail!("Other query returned an unexpected response"),
    }
    Ok(())
}

pub(super) fn list_playlists(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let _ = session.get_state(StateSections::default())?;
    let mut expected = None;
    let mut after = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        write!(output, "[")?;
    }
    let mut first = true;
    loop {
        let response = session.query(Query::PlaylistSummaryBatch {
            query: None,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = listing_result(i18n, response.result)?;
        let ViewResponse::PlaylistSummaryBatch(batch) = view else {
            bail!("Playlist query returned an unexpected response");
        };
        for playlist in &batch.rows {
            if json {
                if !first {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, playlist)?;
                first = false;
            } else {
                writeln!(
                    output,
                    "{}",
                    i18n.format(
                        "core-playlist-summary",
                        &[
                            ("id", playlist.id.to_string().into()),
                            ("name", terminal_text(&playlist.name).into()),
                            ("count", playlist.entry_count.to_string().into()),
                        ]
                    )
                )?;
            }
        }
        if batch.rows.is_empty() && batch.next.is_some() {
            bail!("Playlist query returned an empty batch with a continuation cursor");
        }
        let Some(next) = batch.next else {
            break;
        };
        if after == Some(next) {
            bail!("Playlist query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

pub(super) fn show_playlist(
    i18n: &Localizer,
    data_dir: &Path,
    playlist_id: i64,
    json: bool,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let _ = session.get_state(StateSections::default())?;
    let mut expected = None;
    let mut after = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        write!(output, "[")?;
    }
    let mut index = 0usize;
    let mut first = true;
    loop {
        let response = session.query(Query::PlaylistEntryBatch {
            query: None,
            playlist_id,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = listing_result(i18n, response.result)?;
        let ViewResponse::PlaylistEntryBatch(batch) = view else {
            bail!("Playlist entries query returned an unexpected response");
        };
        for entry in &batch.rows {
            if json {
                if !first {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, entry)?;
                first = false;
            } else {
                writeln!(
                    output,
                    "{}",
                    i18n.format(
                        "core-playlist-entry",
                        &[
                            ("index", index.to_string().into()),
                            ("entry", entry.id.to_string().into()),
                            ("track", entry.track_id.to_string().into()),
                        ]
                    )
                )?;
            }
            index += 1;
        }
        if batch.rows.is_empty() && batch.next.is_some() {
            bail!("Playlist entries query returned an empty batch with a continuation cursor");
        }
        let Some(next) = batch.next else {
            break;
        };
        if after == Some(next) {
            bail!("Playlist entries query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

fn listing_result<T>(i18n: &Localizer, result: std::result::Result<T, QueryError>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(QueryError::RevisionChanged) => bail!("{}", i18n.text("core-listing-changed")),
        Err(error) => Err(anyhow::Error::from(error)),
    }
}

pub(super) fn write_error(
    output: &mut impl Write,
    error: &anyhow::Error,
    json: bool,
) -> Result<()> {
    if json {
        let mut value = serde_json::json!({"error": format!("{error:#}")});
        if let Some(failure) = error.downcast_ref::<rivu_client::CommandFailure>() {
            value["status"] = serde_json::to_value(failure.status)?;
        }
        serde_json::to_writer(&mut *output, &value)?;
        writeln!(output)?;
    } else {
        writeln!(output, "rivu: {:#}", Terminal(error))?;
    }
    Ok(())
}

pub(super) fn show_history(
    output: &mut impl Write,
    entries: &[rivu_protocol::model::HistoryEntry],
    limit: usize,
) -> Result<()> {
    serde_json::to_writer(&mut *output, &entries[..entries.len().min(limit)])?;
    writeln!(output)?;
    Ok(())
}

pub(super) fn show_duplicates(output: &mut impl Write, view: &ViewResponse) -> Result<()> {
    match view {
        ViewResponse::DuplicateGroups(page) => serde_json::to_writer(&mut *output, page)?,
        ViewResponse::DuplicateMembers(page) => serde_json::to_writer(&mut *output, page)?,
        _ => bail!("Duplicate query returned an unexpected response"),
    }
    writeln!(output)?;
    Ok(())
}

pub(super) fn show_ack(
    i18n: &Localizer,
    ack: &rivu_protocol::response::Ack,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(ack)?);
    }
    if !ack.is_success() {
        bail!(
            "{:?}: {}",
            ack.status,
            ack.error.as_deref().unwrap_or("Command failed")
        );
    }
    if !json {
        println!(
            "{}",
            i18n.format(
                "core-command-accepted",
                &[("revision", ack.revision.to_string().into())]
            )
        );
    }
    Ok(())
}
pub(super) fn show_state(i18n: &Localizer, response: &StateResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
        return Ok(());
    }
    let playback = response
        .playback
        .as_ref()
        .context("State omitted playback section")?;
    let library = response
        .library
        .as_ref()
        .context("State omitted library section")?;
    let system = response
        .system
        .as_ref()
        .context("State omitted system section")?;
    let status = match playback.playback.status {
        PlaybackStatus::Stopped => i18n.text("core-stopped"),
        PlaybackStatus::Playing => i18n.text("core-playing"),
        PlaybackStatus::Paused => i18n.text("core-paused"),
    };
    if let Some(track) = playback.current_track.as_deref() {
        println!(
            "{}",
            i18n.format(
                "core-state-track",
                &[
                    ("status", status.into()),
                    ("artist", terminal_text(&track.artist).into()),
                    ("title", terminal_text(&track.title).into()),
                    (
                        "position",
                        format!("{:.1}", playback.playback.position).into()
                    ),
                    (
                        "duration",
                        playback
                            .playback
                            .duration
                            .map_or("?".into(), |v| format!("{v:.1}"))
                            .into()
                    ),
                    (
                        "volume",
                        format!("{:.0}", playback.playback.volume * 100.0).into()
                    ),
                ]
            )
        );
    } else {
        let queued = response.queue.as_ref().map_or(0, |queue| queue.total);
        println!(
            "{}",
            i18n.format(
                "core-state-empty",
                &[
                    ("status", status.into()),
                    ("tracks", library.track_total.to_string().into()),
                    ("queued", queued.to_string().into()),
                ]
            )
        );
    }
    if let Some(progress) = &system.scan_progress {
        show_scan(i18n, progress);
    }
    println!(
        "{}",
        i18n.format(
            "core-ffmpeg",
            &[("status", terminal_text(&system.ffmpeg_status).into())]
        )
    );
    if let Some(error) = &system.last_error {
        eprintln!("{}", Terminal(error));
    }
    Ok(())
}
pub(super) fn show_scan(i18n: &Localizer, progress: &rivu_protocol::model::ScanProgress) {
    let phase = i18n.text(match progress.phase {
        rivu_protocol::model::ScanPhase::Discovering => "core-discovering",
        rivu_protocol::model::ScanPhase::ReadingMetadata => "core-reading-metadata",
        rivu_protocol::model::ScanPhase::Saving => "core-saving",
        rivu_protocol::model::ScanPhase::Completed => "core-completed",
        rivu_protocol::model::ScanPhase::Failed => "core-failed",
        rivu_protocol::model::ScanPhase::Cancelled => "core-cancelled",
    });
    println!(
        "{}",
        i18n.format(
            "core-scan-progress",
            &[
                ("phase", phase.into()),
                ("processed", progress.processed.to_string().into()),
                (
                    "total",
                    progress
                        .total
                        .map_or_else(|| "?".to_owned(), |total| total.to_string())
                        .into()
                ),
                ("errors", progress.errors.to_string().into()),
            ]
        )
    );
    if let Some(path) = &progress.path {
        println!("{}", Terminal(path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::{Terminal, show_history, write_error};
    use std::path::Path;

    #[test]
    fn terminal_display_escapes_all_c0_del_and_c1_controls() {
        for value in (0u32..=0x1f).chain(0x7f..=0x9f) {
            let character = char::from_u32(value).unwrap();
            let expected = match character {
                '\0' => "\\0".to_owned(),
                '\t' => "\\t".to_owned(),
                '\n' => "\\n".to_owned(),
                '\r' => "\\r".to_owned(),
                _ if value <= 0x7f => format!("\\x{value:02x}"),
                _ => format!("\\u{{{value:04x}}}"),
            };
            assert_eq!(Terminal(character).to_string(), expected);
        }
        assert_eq!(
            Terminal("\x1b[31mRed\x1b[0m\u{009b}31m").to_string(),
            "\\x1b[31mRed\\x1b[0m\\u{009b}31m"
        );
    }

    #[test]
    fn terminal_display_preserves_unicode_and_program_separators() {
        let title = "音楽 café e\u{301} 🎵\\literal\tline\nnext\x1b[31m";
        let path = Path::new("音楽/\x1bsong\n.flac");
        assert_eq!(
            format!("7\t{}\t{}\n", Terminal(title), Terminal(path.display())),
            "7\t音楽 café e\u{301} 🎵\\literal\\tline\\nnext\\x1b[31m\t音楽/\\x1bsong\\n.flac\n"
        );
    }

    #[test]
    fn terminal_display_preserves_alternate_error_chain_and_escapes_details() {
        let error = anyhow::anyhow!("bad path 音楽/\x1b[31m\n.flac").context("load\tfailed");
        let mut output = Vec::new();
        write_error(&mut output, &error, false).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "rivu: load\\tfailed: bad path 音楽/\\x1b[31m\\n.flac\n"
        );
    }

    #[test]
    fn json_runtime_errors_preserve_transport_certainty_without_fabricated_fields() {
        use rivu_protocol::response::AckStatus;
        for status in [AckStatus::Unknown, AckStatus::NotAdmitted] {
            let error = anyhow::Error::new(rivu_client::CommandFailure { status })
                .context("send\tfailed\n");
            let mut output = Vec::new();
            write_error(&mut output, &error, true).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["status"], serde_json::to_value(status).unwrap());
            assert_eq!(value["error"], format!("{error:#}"));
            assert_eq!(value.as_object().unwrap().len(), 2);
            assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
        }
        let mut output = Vec::new();
        write_error(&mut output, &anyhow::anyhow!("query failed"), true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value, serde_json::json!({"error": "query failed"}));
    }

    #[test]
    fn history_json_is_a_bounded_array_in_retained_order() {
        use rivu_protocol::model::HistoryEntry;
        let entries = (0..3)
            .map(|id| HistoryEntry {
                track_id: id,
                title: format!("Track {id}"),
                play_count: 1,
                played_at: 100 - id,
                favorite: false,
            })
            .collect::<Vec<_>>();
        for (limit, count) in [(0, 0), (2, 2), (usize::MAX, 3)] {
            let mut output = Vec::new();
            show_history(&mut output, &entries, limit).unwrap();
            let rows: Vec<HistoryEntry> = serde_json::from_slice(&output).unwrap();
            assert_eq!(rows.len(), count);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(row.track_id, entries[index].track_id);
                assert_eq!(row.played_at, entries[index].played_at);
            }
        }
    }
}
