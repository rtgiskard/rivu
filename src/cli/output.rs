use anyhow::{Context, Result, bail};
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{
    model::{Batch, MAX_QUERY_ROWS, PlaybackStatus, Query},
    response::{QueryError, QueryRevisions, StateResponse, StateSections, ViewResponse},
    wire::{CommandOutput, CommandReceipt, CommandReply},
};
use rivu_utils::terminal::{TerminalEscaped, terminal_text};
use std::{
    io::{self, Write},
    path::Path,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputMode {
    Text,
    Verbose,
    Json,
}

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
                row.track.id,
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
        ViewResponse::LibraryMatchCounts {
            other: Some(total), ..
        } => writeln!(output, "{total}")?,
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
        let command_status = error
            .downcast_ref::<rivu_client::CommandFailure>()
            .map(|failure| failure.status)
            .or_else(|| {
                error.downcast_ref::<rivu_protocol::wire::CommandRefusal>()?;
                Some(rivu_protocol::response::AckStatus::NotAdmitted)
            });
        if let Some(status) = command_status {
            value["status"] = serde_json::to_value(status)?;
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
    ack: &CommandReceipt,
    mode: OutputMode,
) -> Result<()> {
    let (kind, id) = match ack.output() {
        Some(CommandOutput::Scan(id)) => ("scan", *id),
        Some(CommandOutput::Relocation(id)) => ("relocation", *id),
        Some(CommandOutput::Processing(id)) => ("processing", *id),
        _ => return show_ack(i18n, ack, mode),
    };
    let instance_id = super::tasks::instance_id(session)?;
    let mut output = io::stdout().lock();
    if mode == OutputMode::Json {
        let mut value = serde_json::to_value(receipt_json(ack))?;
        let fields = value
            .as_object_mut()
            .context("Expected acknowledgement object")?;
        fields.insert("instance_id".into(), instance_id.into());
        let reference = super::tasks::TaskReference { instance_id, id }.to_string();
        fields.insert("task_refs".into(), serde_json::json!({ (kind): reference }));
        writeln!(output, "{}", serde_json::to_string_pretty(&value)?)?;
        output.flush()?;
        return check_ack(ack);
    }
    check_ack(ack)?;
    if mode == OutputMode::Verbose {
        write_ack_revision(&mut output, i18n, ack)?;
    }
    writeln!(
        output,
        "{}",
        i18n.format(
            "core-task-accepted",
            &[
                ("kind", kind.into()),
                (
                    "id",
                    super::tasks::TaskReference { instance_id, id }
                        .to_string()
                        .into()
                ),
            ]
        )
    )?;
    output.flush()?;
    Ok(())
}

pub(super) fn require_receipt(reply: CommandReply) -> Result<CommandReceipt> {
    match reply {
        CommandReply::Receipt(receipt) => Ok(receipt),
        CommandReply::Refused(refusal) => Err(refusal.into()),
    }
}

// Only the JSON renderer needs the established flat acknowledgement schema.
#[derive(serde::Serialize)]
struct ReceiptJson<'a> {
    status: rivu_protocol::response::AckStatus,
    error: Option<&'a str>,
    revision: u64,
    scan_id: Option<u64>,
    relocation_id: Option<u64>,
    processing_id: Option<u64>,
    database_optimization: Option<&'a rivu_protocol::model::DatabaseOptimization>,
    source_tags: Option<&'a rivu_format::tags::TagValues>,
    source_file_saved: bool,
    source_refresh_error: Option<&'a str>,
}

fn receipt_json(receipt: &CommandReceipt) -> ReceiptJson<'_> {
    let source = receipt.outcome.source_tags();
    let mut json = ReceiptJson {
        status: receipt.status(),
        error: receipt.error(),
        revision: receipt.revision,
        scan_id: None,
        relocation_id: None,
        processing_id: None,
        database_optimization: None,
        source_tags: source.and_then(|source| source.tags.as_ref()),
        source_file_saved: source.is_some_and(|source| source.file_saved),
        source_refresh_error: source.and_then(|source| source.refresh_error.as_deref()),
    };
    match receipt.output() {
        Some(CommandOutput::Scan(id)) => json.scan_id = Some(*id),
        Some(CommandOutput::Relocation(id)) => json.relocation_id = Some(*id),
        Some(CommandOutput::Processing(id)) => json.processing_id = Some(*id),
        Some(CommandOutput::DatabaseOptimization(report)) => {
            json.database_optimization = Some(report)
        }
        _ => {}
    }
    json
}

pub(super) fn show_ack(i18n: &Localizer, ack: &CommandReceipt, mode: OutputMode) -> Result<()> {
    if mode == OutputMode::Text {
        return check_ack(ack);
    }
    let mut output = io::stdout().lock();
    if mode == OutputMode::Json {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(&receipt_json(ack))?
        )?;
        output.flush()?;
        return check_ack(ack);
    }
    check_ack(ack)?;
    write_ack_revision(&mut output, i18n, ack)?;
    output.flush()?;
    Ok(())
}

fn check_ack(ack: &CommandReceipt) -> Result<()> {
    if !ack.is_success() {
        bail!(
            "{:?}: {}",
            ack.status(),
            ack.error().unwrap_or("Command failed")
        );
    }
    Ok(())
}

fn write_ack_revision(
    output: &mut impl Write,
    i18n: &Localizer,
    ack: &CommandReceipt,
) -> io::Result<()> {
    writeln!(
        output,
        "{}",
        i18n.format(
            "core-command-accepted",
            &[("revision", ack.revision.to_string().into())]
        )
    )
}

fn write_status_json(output: &mut impl Write, response: &StateResponse) -> Result<()> {
    serde_json::to_writer_pretty(&mut *output, response)?;
    writeln!(output)?;
    Ok(())
}

pub(super) fn show_state(
    i18n: &Localizer,
    response: &StateResponse,
    mode: OutputMode,
) -> Result<()> {
    let mut output = io::stdout().lock();
    if mode == OutputMode::Json {
        let mut output = io::BufWriter::new(output);
        write_status_json(&mut output, response)?;
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
    if let Some(track) = response
        .current_track
        .as_ref()
        .and_then(|source| source.track.as_deref())
    {
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
    if playback.playback.speed != 1.0 {
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-state-speed",
                &[("speed", format!("{:.2}", playback.playback.speed).into())]
            )
        )?;
    }
    if mode == OutputMode::Verbose
        && let Some(state) = playback.playback.audio_output.as_deref()
    {
        writeln!(output, "{}:", i18n.text("audio-output"))?;
        rivu_presentation::audio_output::try_visit_output_fields(state, i18n, |key, value| {
            writeln!(output, "  {}: {}", i18n.text(key), TerminalEscaped(&value))
        })?;
        writeln!(output, "  {}", i18n.text("audio-output-note"))?;
    }
    if let Some(progress) = &system.scan_progress {
        show_scan(i18n, progress)?;
    }
    if mode == OutputMode::Verbose {
        writeln!(
            output,
            "{}",
            i18n.format(
                "core-ffmpeg",
                &[("status", terminal_text(&system.ffmpeg_status).into())]
            )
        )?;
        if let Some(error) = &system.last_error {
            writeln!(
                output,
                "{}",
                i18n.format(
                    "core-status-last-error",
                    &[("error", terminal_text(error).into())]
                )
            )?;
        }
    }
    output.flush()?;
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

    #[test]
    fn status_json_uses_protocol_sections_and_tokens() {
        use rivu_protocol::{model::RepeatMode, response::*};
        let response = StateResponse {
            revisions: StateRevisions {
                playback: 7,
                current_track: 11,
                configuration: 19,
                ..Default::default()
            },
            playback: Some(PlaybackSnapshot {
                playback: rivu_protocol::model::PlaybackState {
                    volume: 0.25,
                    shuffle: true,
                    repeat: RepeatMode::All,
                    ..Default::default()
                },
                processing_audition: None,
                processing_selection: None,
            }),
            current_track: Some(CurrentTrackState {
                track: None,
                source_generation: 13,
            }),
            configuration: Some(Default::default()),
            system: Some(rivu_protocol::model::SystemState {
                selected_device: Some("live output".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        super::write_status_json(&mut bytes, &response).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["revisions"]["playback"], 7);
        assert_eq!(json["revisions"]["current_track"], 11);
        assert_eq!(json["revisions"]["configuration"], 19);
        assert_eq!(json["current_track"]["source_generation"], 13);
        assert_eq!(json["playback"]["playback"]["volume"], 0.25);
        assert_eq!(json["system"]["selected_device"], "live output");
        assert!(json["configuration"]["config"].get("volume").is_none());
    }

    #[test]
    fn command_receipt_json_retains_legacy_fields_and_refusals_have_no_revision() {
        use rivu_protocol::wire::{
            CommandOutcome, CommandOutput, CommandReceipt, CommandRefusal, CommandReply,
        };

        let reply = CommandReply::Receipt(CommandReceipt {
            revision: 7,
            outcome: CommandOutcome::Succeeded(CommandOutput::Scan(3)),
        });
        let value =
            serde_json::to_value(super::receipt_json(&super::require_receipt(reply).unwrap()))
                .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "status": "Succeeded", "error": null, "revision": 7,
                "scan_id": 3, "relocation_id": null, "processing_id": null,
                "database_optimization": null, "source_tags": null,
                "source_file_saved": false, "source_refresh_error": null,
            })
        );

        let refused =
            CommandReply::Refused(CommandRefusal::InvalidCommand("invalid command".into()));
        let error = super::require_receipt(refused).unwrap_err();
        let mut bytes = Vec::new();
        write_error(&mut bytes, &error, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"status": "NotAdmitted", "error": "invalid command"})
        );
    }

    #[test]
    fn command_receipt_json_distinguishes_partial_failure_and_unknown() {
        use rivu_protocol::wire::{CommandOutcome, CommandReceipt, SourceTagOutput};

        let tags = rivu_format::tags::TagValues {
            title: "Verified".into(),
            ..Default::default()
        };
        let partial = CommandReceipt {
            revision: 8,
            outcome: CommandOutcome::Failed {
                error: "database sync failed".into(),
                source_tags: Some(SourceTagOutput {
                    tags: Some(tags.clone()),
                    file_saved: true,
                    refresh_error: None,
                }),
            },
        };
        let value = serde_json::to_value(super::receipt_json(&partial)).unwrap();
        assert_eq!(value["status"], "Failed");
        assert_eq!(value["error"], "database sync failed");
        assert_eq!(value["source_file_saved"], true);
        assert_eq!(value["source_tags"], serde_json::to_value(tags).unwrap());
        assert_eq!(
            super::check_ack(&partial).unwrap_err().to_string(),
            "Failed: database sync failed"
        );

        let unknown = CommandReceipt {
            revision: 9,
            outcome: CommandOutcome::Unknown("do not automatically retry".into()),
        };
        let value = serde_json::to_value(super::receipt_json(&unknown)).unwrap();
        assert_eq!(value["status"], "Unknown");
        assert_eq!(value["revision"], 9);
        assert_eq!(value["source_file_saved"], false);
        assert!(value["source_tags"].is_null());
        assert_eq!(
            super::check_ack(&unknown).unwrap_err().to_string(),
            "Unknown: do not automatically retry"
        );
    }

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

    #[cfg(feature = "test-support")]
    #[test]
    fn queue_listing_rejects_a_content_edit_before_the_first_batch() {
        use rivu::test_support::{
            catalog::seed_playlist_targets, frontend::FrontendFixture, ipc::socket_test_directory,
        };
        use rivu_protocol::model::{Command, Query};

        let directory = socket_test_directory();
        let path = directory.path();
        seed_playlist_targets(path, 0).unwrap();
        let _service = FrontendFixture::start(path, &path.join("config.toml")).unwrap();
        let i18n = rivu_presentation::i18n::Localizer::for_cli("en-US");
        super::list_queue(&i18n, path, false).unwrap();
        let mut session = rivu_client::client_session(path);
        let initial = session
            .get_state(super::StateSections {
                queue: true,
                ..Default::default()
            })
            .unwrap()
            .queue
            .unwrap()
            .revision;
        let reply = session
            .request_command(Command::Enqueue { track_ids: vec![1] })
            .unwrap();
        assert!(reply.is_success());
        let response = session
            .query(Query::QueueBatch {
                query: None,
                after: None,
                limit: 1,
                expected: None,
            })
            .unwrap();
        let super::ViewResponse::QueueBatch(batch) = response.result.unwrap() else {
            panic!("expected a queue batch");
        };
        assert_eq!(batch.rows[0].track.id, 1);
        let error =
            super::validate_queue_listing(&i18n, initial, response.revisions.queue).unwrap_err();
        assert_eq!(error.to_string(), i18n.text("core-listing-changed"));
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
