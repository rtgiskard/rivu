use super::output::{list_library, show_ack};
use super::{
    args::{LibraryAction, OtherTracksAction, Switch},
    client_path, output, submit_command, tasks,
};
use anyhow::{Context, Result, bail};
use rivu::config::ConfigPersistence;
use rivu::{config, i18n::Localizer};
use rivu_audio as audio;
use rivu_protocol::{
    model::{Command, Query},
    response::{StateSections, ViewResponse},
};
use rivu_utils::terminal::{TerminalEscaped, terminal_text};
use std::path::Path;

pub(super) fn run(
    i18n: &Localizer,
    data_dir: &Path,
    config_path: &Path,
    action: LibraryAction,
    json: bool,
) -> Result<()> {
    match action {
        LibraryAction::Probe { path, enhanced } => {
            let probe_config = config::Config::load(config_path)?;
            let ffmpeg_enabled = probe_config.ffmpeg_enabled || enhanced;
            let result = audio::probe_details(&path, ffmpeg_enabled, enhanced)?;
            let info = result.info;
            let decoder_status = if ffmpeg_enabled {
                audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
            } else {
                "disabled".to_owned()
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": path, "title": info.title, "artist": info.artist, "album": info.album,
                        "codec": info.codec, "channels": info.channels, "sample_rate": info.sample_rate,
                        "duration": info.duration, "ffmpeg_enabled": ffmpeg_enabled,
                        "bitrate_bps": info.bitrate_bps, "bits_per_sample": info.bits_per_sample,
                        "bitrate_exact": info.bitrate_exact,
                        "track_number": info.track_number, "disc_number": info.disc_number,
                        "release_date": info.release_date,
                        "ffmpeg_status": decoder_status,
                        "probe_backend": result.probe_backend,
                        "decoder_backend": result.decoder_backend,
                    }))?
                );
                return Ok(());
            }
            println!(
                "{}",
                i18n.format(
                    "core-probe",
                    &[
                        ("path", TerminalEscaped(path.display()).to_string().into()),
                        ("title", terminal_text(&info.title).into()),
                        ("artist", terminal_text(&info.artist).into()),
                        ("album", terminal_text(&info.album).into()),
                        ("codec", terminal_text(&info.codec).into()),
                        ("channels", info.channels.to_string().into()),
                        ("sample-rate", info.sample_rate.to_string().into()),
                        (
                            "duration",
                            info.duration
                                .map_or_else(
                                    || i18n.text("core-unknown").to_owned(),
                                    |value| format!("{value:.3} s")
                                )
                                .into()
                        ),
                        (
                            "bitrate",
                            info.bitrate_bps
                                .map_or("—".into(), |value| format!(
                                    "{}{:.1} kbps",
                                    if info.bitrate_exact { "" } else { "≈" },
                                    value as f64 / 1000.0
                                ))
                                .into()
                        ),
                        (
                            "bits",
                            info.bits_per_sample
                                .map_or("—".into(), |v| v.to_string())
                                .into()
                        ),
                        (
                            "track",
                            info.track_number
                                .map_or("—".into(), |v| v.to_string())
                                .into()
                        ),
                        (
                            "disc",
                            info.disc_number
                                .map_or("—".into(), |v| v.to_string())
                                .into()
                        ),
                        (
                            "date",
                            terminal_text(info.release_date.as_deref().unwrap_or("—")).into()
                        ),
                        ("probe", result.probe_backend.to_string().into()),
                        ("decoder", result.decoder_backend.to_string().into()),
                        ("ffmpeg", terminal_text(&decoder_status).into()),
                    ]
                )
            );
            Ok(())
        }
        LibraryAction::List {
            query,
            favorite,
            missing,
        } => list_library(i18n, data_dir, json, query, favorite, missing),
        LibraryAction::Other(
            action @ (OtherTracksAction::List { .. }
            | OtherTracksAction::Count
            | OtherTracksAction::Locate { .. }),
        ) => {
            let query = other_tracks_query(action)?;
            let view = rivu_client::query(data_dir, query)?
                .result
                .map_err(anyhow::Error::from)?;
            output::show_other_tracks(i18n, view, json)
        }
        LibraryAction::Stats => {
            let response = rivu_client::query(data_dir, Query::LibraryStats)?;
            let ViewResponse::LibraryStats(stats) = response.result.map_err(anyhow::Error::from)?
            else {
                bail!("Statistics query returned an unexpected response");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!(
                    "{}",
                    i18n.format(
                        "core-stats",
                        &[
                            ("tracks", stats.total.to_string().into()),
                            ("plays", stats.play_count.to_string().into())
                        ]
                    )
                );
            }
            Ok(())
        }
        LibraryAction::History { limit } => {
            let response = rivu_client::get_state(
                data_dir,
                StateSections {
                    library: true,
                    ..StateSections::default()
                },
            )?;
            let state = response
                .library
                .as_ref()
                .context("State omitted library section")?;
            if json {
                return output::show_history(&mut std::io::stdout().lock(), &state.history, limit);
            }
            for entry in state.history.iter().take(limit) {
                println!(
                    "{}\t{}\t{}",
                    entry.played_at,
                    entry.track_id,
                    TerminalEscaped(&entry.title)
                );
            }
            Ok(())
        }
        LibraryAction::Edit {
            track_id,
            title,
            artist,
            album,
        } => {
            let command = Command::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint: None,
            };
            let ack = rivu_client::request_ack(data_dir, command)?;
            show_ack(i18n, &ack, json)
        }
        LibraryAction::Duplicates {
            fingerprint,
            offset,
            limit,
        } => {
            let query = match fingerprint {
                Some(fingerprint) => Query::DuplicateMembers {
                    fingerprint,
                    offset,
                    limit,
                },
                None => Query::DuplicateGroups { offset, limit },
            };
            let view = rivu_client::query(data_dir, query)?
                .result
                .map_err(anyhow::Error::from)?;
            if json {
                output::show_duplicates(&mut std::io::stdout().lock(), &view)?;
            } else {
                match view {
                    ViewResponse::DuplicateGroups(page) => {
                        println!(
                            "{}",
                            i18n.format(
                                "core-duplicate-groups",
                                &[("count", page.total.to_string().into())]
                            )
                        );
                        for group in page.rows {
                            println!(
                                "{}",
                                i18n.format(
                                    "core-duplicate-group",
                                    &[
                                        ("fingerprint", terminal_text(&group.fingerprint).into()),
                                        ("total", group.total_members.to_string().into()),
                                        ("missing", group.missing_members.to_string().into()),
                                        ("present", group.present_members.to_string().into()),
                                    ]
                                )
                            );
                        }
                    }
                    ViewResponse::DuplicateMembers(page) => {
                        println!(
                            "{}",
                            i18n.format(
                                "core-duplicate-members",
                                &[("count", page.total.to_string().into())]
                            )
                        );
                        for member in page.rows {
                            println!(
                                "{}",
                                i18n.format(
                                    "core-duplicate-member",
                                    &[
                                        ("id", member.track.id.to_string().into()),
                                        (
                                            "path",
                                            TerminalEscaped(member.track.path.display())
                                                .to_string()
                                                .into()
                                        ),
                                        (
                                            "status",
                                            i18n.text(if member.track.missing {
                                                "core-last-scan-missing"
                                            } else {
                                                "core-last-scan-present"
                                            })
                                            .into()
                                        ),
                                        (
                                            "favorite",
                                            i18n.text(if member.track.favorite {
                                                "core-yes"
                                            } else {
                                                "core-no"
                                            })
                                            .into()
                                        ),
                                        (
                                            "overrides",
                                            i18n.text(if member.has_overrides {
                                                "core-yes"
                                            } else {
                                                "core-no"
                                            })
                                            .into()
                                        ),
                                        ("plays", member.track.play_count.to_string().into()),
                                        (
                                            "playlists",
                                            member.playlist_references.to_string().into()
                                        ),
                                        ("queue", member.queue_references.to_string().into()),
                                        (
                                            "independent",
                                            i18n.text(if member.independent {
                                                "core-yes"
                                            } else {
                                                "core-no"
                                            })
                                            .into()
                                        ),
                                    ]
                                )
                            );
                        }
                    }
                    _ => bail!("Duplicate query returned an unexpected response"),
                }
            }
            Ok(())
        }
        LibraryAction::Relocate {
            source_id,
            target_id,
            wait,
        } => {
            let mut session = rivu_client::client_session(data_dir)?;
            let ack = session.request_ack(Command::RelocateTrack {
                source_id,
                target_id,
            })?;
            if !ack.is_success() || !wait {
                return output::show_task_ack(i18n, &session, &ack, json);
            }
            let id = ack
                .relocation_id
                .context("Relocation startup omitted task ID")?;
            tasks::wait_relocation(i18n, &mut session, id, json)
        }
        LibraryAction::Optimize => {
            let ack = rivu_client::request_ack(data_dir, Command::OptimizeDatabase)?;
            if json || !ack.is_success() {
                return show_ack(i18n, &ack, json);
            }
            let report = ack
                .database_optimization
                .as_ref()
                .context("Database optimization returned no size report")?;
            let before = report
                .database_bytes_before
                .saturating_add(report.wal_bytes_before);
            let after = report
                .database_bytes_after
                .saturating_add(report.wal_bytes_after);
            println!(
                "{}",
                i18n.format(
                    "core-optimization",
                    &[
                        ("before", report.database_bytes_before.to_string().into()),
                        ("after", report.database_bytes_after.to_string().into()),
                        ("wal-before", report.wal_bytes_before.to_string().into()),
                        ("wal-after", report.wal_bytes_after.to_string().into()),
                        ("reclaimed", before.saturating_sub(after).to_string().into()),
                    ]
                )
            );
            Ok(())
        }
        action => {
            let (command, wait) = translate(action)?;
            submit_command(i18n, data_dir, command, wait, json)
        }
    }
}

pub(super) fn other_tracks_query(action: OtherTracksAction) -> Result<Query> {
    Ok(match action {
        OtherTracksAction::List {
            filter,
            offset,
            limit,
        } => Query::OtherTracksPage {
            query: filter.query,
            favorite: filter.favorite,
            missing: filter.missing,
            sort: filter.sort.into(),
            offset,
            limit,
        },
        OtherTracksAction::Count => Query::OtherTracksCount,
        OtherTracksAction::Locate { track_id, filter } => Query::OtherTracksLocate {
            track_id,
            query: filter.query,
            favorite: filter.favorite,
            missing: filter.missing,
            sort: filter.sort.into(),
        },
        OtherTracksAction::Check { .. } => bail!("File checks are commands, not queries"),
    })
}

pub(super) fn translate(action: LibraryAction) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
        LibraryAction::Optimize => Command::OptimizeDatabase,
        LibraryAction::Scan {
            paths,
            wait: should_wait,
            force,
        } => {
            wait = should_wait;
            Command::Scan {
                paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                force,
            }
        }
        LibraryAction::Other(OtherTracksAction::Check {
            wait: should_wait,
            force,
        }) => {
            wait = should_wait;
            Command::CheckOtherTracks { force }
        }
        LibraryAction::Remove { track_ids } => Command::RemoveTracks { track_ids },
        LibraryAction::Favorite { track_ids, mode } => Command::SetFavorite {
            track_ids,
            favorite: matches!(mode, Switch::On),
        },
        LibraryAction::CleanMissing => Command::RemoveMissingTracks,
        LibraryAction::KeepIndependent { track_id } => Command::KeepTrackIndependent { track_id },
        LibraryAction::List { .. }
        | LibraryAction::Other(_)
        | LibraryAction::Edit { .. }
        | LibraryAction::Stats
        | LibraryAction::History { .. }
        | LibraryAction::Probe { .. }
        | LibraryAction::Duplicates { .. }
        | LibraryAction::Relocate { .. } => bail!("This action is not a library command"),
    };
    Ok((command, wait))
}
