use anyhow::{Context, Result, bail};
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{
    model::{Batch, MAX_QUERY_ROWS, PlaybackStatus, Query},
    response::{QueryError, QueryRevisions, StateResponse, StateSections, ViewResponse},
};
use rivu_utils::terminal::{TerminalEscaped, terminal_text};
use std::{
    io::{self, Write},
    path::Path,
};

pub(super) fn list_queue(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
    let state = session.get_state(StateSections {
        queue: true,
        ..StateSections::default()
    })?;
    let queue = state.queue.context("State omitted queue section")?;
    write_listing(
        i18n,
        &mut session,
        json,
        false,
        |after, expected| Query::QueueBatch {
            query: None,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        },
        |view, revisions| {
            validate_queue_listing(i18n, queue.revision, revisions.queue)?;
            Ok(match view {
                ViewResponse::QueueBatch(batch) => Some(batch),
                _ => None,
            })
        },
        |output, row, _| {
            writeln!(
                output,
                "{}\t{}\t{}\t{}{}",
                row.position,
                row.id,
                row.track_id,
                TerminalEscaped(&row.track.title),
                if queue.current_id == Some(row.id) {
                    " *"
                } else {
                    ""
                }
            )?;
            Ok(())
        },
    )
}

pub(super) fn list_library(
    i18n: &Localizer,
    data_dir: &Path,
    json: bool,
    query: Option<String>,
    favorite: Option<bool>,
    missing: Option<bool>,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
    let _ = session.get_state(StateSections::default())?;
    write_listing(
        i18n,
        &mut session,
        json,
        false,
        |after, expected| Query::TrackBatch {
            query: query.clone(),
            favorite,
            missing,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        },
        |view, _| {
            Ok(match view {
                ViewResponse::TrackBatch(batch) => Some(batch),
                _ => None,
            })
        },
        |output, track, _| {
            write!(
                output,
                "{}\t{}\t{}\t{}\t{}{}{}\t",
                track.id,
                TerminalEscaped(&track.title),
                TerminalEscaped(&track.artist),
                TerminalEscaped(&track.album),
                TerminalEscaped(track.path.display()),
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
            Ok(())
        },
    )
}

pub(super) fn show_other_tracks(i18n: &Localizer, view: ViewResponse, json: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    match view {
        ViewResponse::LibraryPage(page) => {
            if json {
                writeln!(output, "{}", serde_json::to_string_pretty(&page)?)?;
            } else {
                writeln!(output, "total\t{}", page.total)?;
                for track in page.rows {
                    writeln!(
                        output,
                        "{}\t{}\t{}\t{}\t{}{}{}",
                        track.id,
                        TerminalEscaped(&track.title),
                        TerminalEscaped(&track.artist),
                        TerminalEscaped(&track.album),
                        TerminalEscaped(track.path.display()),
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
                    )?;
                }
            }
        }
        ViewResponse::OtherTracksCount(count) => writeln!(output, "{count}")?,
        ViewResponse::LibraryLocate(location) => {
            if json {
                writeln!(output, "{}", serde_json::to_string_pretty(&location)?)?;
            } else {
                use rivu_protocol::model::LibraryLocateResult;
                match location {
                    LibraryLocateResult::Located(location) => {
                        writeln!(output, "located\t{}\t{}", location.offset, location.total)?;
                    }
                    LibraryLocateResult::Filtered => writeln!(output, "filtered")?,
                    LibraryLocateResult::Unavailable => writeln!(output, "unavailable")?,
                    LibraryLocateResult::OutsideRoot => writeln!(output, "outside-root")?,
                }
            }
        }
        _ => bail!("Other query returned an unexpected response"),
    }
    output.flush()?;
    Ok(())
}

pub(super) fn list_playlists(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
    let _ = session.get_state(StateSections::default())?;
    write_listing(
        i18n,
        &mut session,
        json,
        false,
        |after, expected| Query::PlaylistSummaryBatch {
            query: None,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        },
        |view, _| {
            Ok(match view {
                ViewResponse::PlaylistSummaryBatch(batch) => Some(batch),
                _ => None,
            })
        },
        |output, playlist, _| {
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
            Ok(())
        },
    )
}

pub(super) fn show_playlist(
    i18n: &Localizer,
    data_dir: &Path,
    playlist_id: i64,
    json: bool,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
    let _ = session.get_state(StateSections::default())?;
    write_listing(
        i18n,
        &mut session,
        json,
        true,
        |after, expected| Query::PlaylistEntryBatch {
            query: None,
            playlist_id,
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        },
        |view, _| {
            Ok(match view {
                ViewResponse::PlaylistEntryBatch(batch) => Some(batch),
                _ => None,
            })
        },
        |output, entry, index| {
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
            Ok(())
        },
    )
}

/// Stream one revision-pinned traversal without retaining previously printed rows.
fn write_listing<T: serde::Serialize, C: Copy + PartialEq>(
    i18n: &Localizer,
    session: &mut rivu_client::ClientSession,
    json: bool,
    include_playlist_revision: bool,
    mut query: impl FnMut(Option<C>, Option<QueryRevisions>) -> Query,
    unpack: impl Fn(ViewResponse, QueryRevisions) -> Result<Option<Batch<T, C>>>,
    mut text: impl FnMut(&mut dyn Write, &T, usize) -> Result<()>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut after = None;
    let mut expected = None;
    let mut index = 0;
    loop {
        let response = session.query(query(after, expected))?;
        let batch = unpack(listing_result(i18n, response.result)?, response.revisions)?
            .context("Listing query returned an unexpected response")?;
        if expected.is_none() {
            if include_playlist_revision {
                let revision = response
                    .revisions
                    .playlist_entries
                    .context("Playlist listing omitted its entry revision")?;
                write_playlist_heading(&mut output, json, revision)?;
            } else if json {
                write!(output, "[")?;
            }
        }
        expected.get_or_insert(response.revisions);
        for row in &batch.rows {
            if json {
                if index != 0 {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, row)?;
            } else {
                text(&mut output, row, index)?;
            }
            index += 1;
        }
        let Some(next) = batch.next else { break };
        if batch.rows.is_empty() || after == Some(next) {
            bail!("Listing query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        if include_playlist_revision {
            writeln!(output, "]}}")?;
        } else {
            writeln!(output, "]")?;
        }
    }
    output.flush()?;
    Ok(())
}

fn write_playlist_heading(
    output: &mut dyn Write,
    json: bool,
    revision: rivu_protocol::response::PlaylistEntryRevision,
) -> Result<()> {
    if json {
        write!(
            output,
            "{{\"playlist_id\":{},\"revision\":{},\"entries\":[",
            revision.playlist_id, revision.revision
        )?;
    } else {
        writeln!(
            output,
            "playlist_id={} revision={}",
            revision.playlist_id, revision.revision
        )?;
    }
    Ok(())
}

fn validate_queue_listing(i18n: &Localizer, initial: u64, observed: u64) -> Result<()> {
    listing_result(
        i18n,
        if initial == observed {
            Ok(())
        } else {
            Err(QueryError::RevisionChanged)
        },
    )
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
        let mut value = if let Some(failure) = error.downcast_ref::<rivu_metadata::EditFailure>() {
            serde_json::to_value(failure)?
        } else {
            serde_json::json!({"error": format!("{error:#}")})
        };
        if let Some(failure) = error.downcast_ref::<rivu_client::CommandFailure>() {
            value["status"] = serde_json::to_value(failure.status)?;
        }
        serde_json::to_writer(&mut *output, &value)?;
        writeln!(output)?;
    } else {
        writeln!(output, "rivu: {:#}", TerminalEscaped(error))?;
    }
    output.flush()?;
    Ok(())
}

pub(super) fn show_history(
    output: &mut impl Write,
    entries: &[rivu_protocol::model::HistoryEntry],
    limit: usize,
) -> Result<()> {
    serde_json::to_writer(&mut *output, &entries[..entries.len().min(limit)])?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

pub(super) fn show_duplicates(output: &mut impl Write, view: &ViewResponse) -> Result<()> {
    match view {
        ViewResponse::DuplicateGroups(page) => serde_json::to_writer(&mut *output, page)?,
        ViewResponse::DuplicateMembers(page) => serde_json::to_writer(&mut *output, page)?,
        _ => bail!("Duplicate query returned an unexpected response"),
    }
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

pub(super) fn show_task_ack(
    i18n: &Localizer,
    session: &rivu_client::ClientSession,
    ack: &rivu_protocol::response::Ack,
    json: bool,
) -> Result<()> {
    let ids = [
        ("scan", ack.scan_id),
        ("relocation", ack.relocation_id),
        ("processing", ack.processing_id),
    ];
    if !ids.iter().any(|(_, id)| id.is_some()) {
        return show_ack(i18n, ack, json);
    }
    let mut output = io::stdout().lock();
    let instance_id = super::tasks::instance_id(session)?;
    if json {
        let mut value = serde_json::to_value(ack)?;
        let fields = value
            .as_object_mut()
            .context("Expected acknowledgement object")?;
        fields.insert("instance_id".into(), instance_id.into());
        let references: serde_json::Map<String, serde_json::Value> = ids
            .into_iter()
            .filter_map(|(kind, id)| {
                id.map(|id| {
                    (
                        kind.into(),
                        super::tasks::TaskReference { instance_id, id }
                            .to_string()
                            .into(),
                    )
                })
            })
            .collect();
        fields.insert("task_refs".into(), references.into());
        writeln!(output, "{}", serde_json::to_string_pretty(&value)?)?;
        if !ack.is_success() {
            output.flush()?;
            bail!(
                "{:?}: {}",
                ack.status,
                ack.error.as_deref().unwrap_or("Command failed")
            );
        }
    } else {
        show_ack(i18n, ack, false)?;
        for (kind, id) in ids {
            if let Some(id) = id {
                writeln!(
                    output,
                    "{kind}: {}",
                    super::tasks::TaskReference { instance_id, id }
                )?;
            }
        }
    }
    output.flush()?;
    Ok(())
}

pub(super) fn show_ack(
    i18n: &Localizer,
    ack: &rivu_protocol::response::Ack,
    json: bool,
) -> Result<()> {
    let mut output = io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(ack)?)?;
    }
    if !ack.is_success() {
        output.flush()?;
        bail!(
            "{:?}: {}",
            ack.status,
            ack.error.as_deref().unwrap_or("Command failed")
        );
    }
    if !json {
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-command-accepted",
                &[("revision", ack.revision.to_string().into())]
            )
        )?;
    }
    output.flush()?;
    Ok(())
}
pub(super) fn show_state(i18n: &Localizer, response: &StateResponse, json: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(response)?)?;
        output.flush()?;
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
        writeln!(
            output,
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
        )?;
    } else {
        let queued = response.queue.as_ref().map_or(0, |queue| queue.total);
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-state-empty",
                &[
                    ("status", status.into()),
                    ("tracks", library.track_total.to_string().into()),
                    ("queued", queued.to_string().into()),
                ]
            )
        )?;
    }
    if let Some(progress) = &system.scan_progress {
        show_scan(i18n, progress)?;
    }
    writeln!(
        output,
        "{}",
        i18n.format(
            "core-ffmpeg",
            &[("status", terminal_text(&system.ffmpeg_status).into())]
        )
    )?;
    output.flush()?;
    if let Some(error) = &system.last_error {
        let mut output = io::stderr().lock();
        writeln!(output, "{}", TerminalEscaped(error))?;
        output.flush()?;
    }
    Ok(())
}
pub(super) fn show_scan(
    i18n: &Localizer,
    progress: &rivu_protocol::model::ScanProgress,
) -> Result<()> {
    let mut output = io::stdout().lock();
    let phase = i18n.text(match progress.phase {
        rivu_protocol::model::ScanPhase::Discovering => "core-discovering",
        rivu_protocol::model::ScanPhase::ReadingMetadata => "core-reading-metadata",
        rivu_protocol::model::ScanPhase::Saving => "core-saving",
        rivu_protocol::model::ScanPhase::Completed => "core-completed",
        rivu_protocol::model::ScanPhase::Failed => "core-failed",
        rivu_protocol::model::ScanPhase::Cancelled => "core-cancelled",
    });
    writeln!(
        output,
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
    )?;
    if let Some(path) = &progress.path {
        writeln!(output, "{}", TerminalEscaped(path.display()))?;
    }
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{show_history, write_error};
    use rivu_utils::terminal::TerminalEscaped;
    use std::path::Path;

    #[test]
    fn buffered_error_output_propagates_real_broken_pipe_on_flush() {
        use std::{io, io::BufWriter, os::unix::net::UnixStream};

        for json in [false, true] {
            let (writer, reader) = UnixStream::pair().unwrap();
            drop(reader);
            let mut output = BufWriter::new(writer);
            let error =
                write_error(&mut output, &anyhow::anyhow!("query failed"), json).unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::BrokenPipe
            );
            assert!(!output.buffer().is_empty());
        }
    }

    #[test]
    fn history_output_propagates_real_broken_pipe_on_flush() {
        use std::{io, io::BufWriter, os::unix::net::UnixStream};

        let (writer, reader) = UnixStream::pair().unwrap();
        drop(reader);
        let mut output = BufWriter::new(writer);
        let error = show_history(&mut output, &[], 0).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(output.buffer(), b"[]\n");
    }

    #[test]
    fn queue_listing_rejects_a_changed_initial_current_snapshot() {
        let i18n = rivu_presentation::i18n::Localizer::for_cli("en-US");
        super::validate_queue_listing(&i18n, 7, 7).unwrap();
        // Current-occurrence changes advance the queue token even if the row
        // order is unchanged, so the first batch must honor the initial token.
        let error = super::validate_queue_listing(&i18n, 7, 8).unwrap_err();
        assert_eq!(error.to_string(), i18n.text("core-listing-changed"));
    }

    #[test]
    fn terminal_display_preserves_unicode_and_program_separators() {
        let title = "音楽 café e\u{301} 🎵\\literal\tline\nnext\x1b[31m";
        let path = Path::new("音楽/\x1bsong\n.flac");
        assert_eq!(
            format!(
                "7\t{}\t{}\n",
                TerminalEscaped(title),
                TerminalEscaped(path.display())
            ),
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

#[cfg(test)]
mod playlist_heading_tests {
    use super::*;

    #[test]
    fn playlist_show_exposes_owner_and_exact_revision_even_without_entries() {
        let revision = rivu_protocol::response::PlaylistEntryRevision {
            playlist_id: 10,
            revision: u64::MAX,
        };
        let mut json = Vec::new();
        write_playlist_heading(&mut json, true, revision).unwrap();
        write!(&mut json, "]}}").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["playlist_id"], 10);
        assert_eq!(value["revision"], u64::MAX);
        assert_eq!(value["entries"], serde_json::json!([]));

        let mut text = Vec::new();
        write_playlist_heading(&mut text, false, revision).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!("playlist_id=10 revision={}\n", u64::MAX)
        );
    }
}
