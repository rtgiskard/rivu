use super::output::{list_library, show_ack};
use super::{
    args::{LibraryAction, OtherTracksAction, Switch},
    client_path, output, submit_command, tasks,
};
use anyhow::{Context, Result, bail};
use rivu::config;
use rivu::config::ConfigPersistence;
use rivu_audio as audio;
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{
    model::{Command, Query},
    response::{StateSections, ViewResponse},
};
use rivu_utils::terminal::{TerminalEscaped, terminal_text};
use std::{io::Write, path::Path};

pub(super) fn run(
    i18n: &Localizer,
    data_dir: &Path,
    config_path: &Path,
    action: LibraryAction,
    json: bool,
) -> Result<()> {
    let mut output = std::io::stdout().lock();
    let result = match action {
        LibraryAction::Tags {
            path,
            title,
            artist,
            album,
        } => {
            let patch = rivu_metadata::TagPatch {
                title,
                artist,
                album,
            };
            if patch.title.is_some() || patch.artist.is_some() || patch.album.is_some() {
                let report = rivu_metadata::edit_file(&path, &patch)?;
                if json {
                    writeln!(output, "{}", serde_json::to_string_pretty(&report)?)?;
                } else if report.changed() {
                    let backup = report
                        .backup_path()
                        .context("Source tag edit omitted its backup path")?;
                    writeln!(
                        output,
                        "{}",
                        i18n.format(
                            "core-tags-written",
                            &[
                                (
                                    "path",
                                    TerminalEscaped(report.path.display()).to_string().into()
                                ),
                                (
                                    "backup",
                                    TerminalEscaped(backup.display()).to_string().into()
                                ),
                            ]
                        )
                    )?;
                } else {
                    writeln!(
                        output,
                        "{}",
                        i18n.format(
                            "core-tags-unchanged",
                            &[(
                                "path",
                                TerminalEscaped(report.path.display()).to_string().into()
                            )]
                        )
                    )?;
                }
                output.flush()?;
                return Ok(());
            }
            let mut file =
                rivu_utils::regular_file::open_regular_file(&path).with_context(|| {
                    format!("Open source tags: {}", TerminalEscaped(path.display()))
                })?;
            let tags = rivu_metadata::read_tags(&mut file)?
                .context("Source tag reading is not supported for this container")?;
            if json {
                writeln!(
                    output,
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": rivu_metadata::JsonPath(&path),
                        "tags": tags,
                    }))?
                )?;
            } else {
                writeln!(
                    output,
                    "{}",
                    i18n.format(
                        "core-tags",
                        &[
                            ("path", TerminalEscaped(path.display()).to_string().into()),
                            (
                                "title",
                                terminal_text(tags.title.as_deref().unwrap_or("—")).into()
                            ),
                            (
                                "artist",
                                terminal_text(tags.artist.as_deref().unwrap_or("—")).into()
                            ),
                            (
                                "album",
                                terminal_text(tags.album.as_deref().unwrap_or("—")).into()
                            ),
                            (
                                "track",
                                tags.track_number
                                    .map_or("—".into(), |value| value.to_string())
                                    .into()
                            ),
                            (
                                "disc",
                                tags.disc_number
                                    .map_or("—".into(), |value| value.to_string())
                                    .into()
                            ),
                            (
                                "date",
                                terminal_text(tags.release_date.as_deref().unwrap_or("—")).into()
                            ),
                        ]
                    )
                )?;
            }
            Ok(())
        }
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
                writeln!(
                    output,
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": rivu_metadata::JsonPath(&path), "title": info.title, "artist": info.artist, "album": info.album,
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
                )?;
                output.flush()?;
                return Ok(());
            }
            writeln!(
                output,
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
            )?;
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
                writeln!(output, "{}", serde_json::to_string_pretty(&stats)?)?;
            } else {
                writeln!(
                    output,
                    "{}",
                    i18n.format(
                        "core-stats",
                        &[
                            ("tracks", stats.total.to_string().into()),
                            ("plays", stats.play_count.to_string().into())
                        ]
                    )
                )?;
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
                return output::show_history(&mut output, &state.history, limit);
            }
            for entry in state.history.iter().take(limit) {
                writeln!(
                    output,
                    "{}\t{}\t{}",
                    entry.played_at,
                    entry.track_id,
                    TerminalEscaped(&entry.title)
                )?;
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
                output::show_duplicates(&mut output, &view)?;
            } else {
                match view {
                    ViewResponse::DuplicateGroups(page) => {
                        writeln!(
                            output,
                            "{}",
                            i18n.format(
                                "core-duplicate-groups",
                                &[("count", page.total.to_string().into())]
                            )
                        )?;
                        for group in page.rows {
                            writeln!(
                                output,
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
                            )?;
                        }
                    }
                    ViewResponse::DuplicateMembers(page) => {
                        writeln!(
                            output,
                            "{}",
                            i18n.format(
                                "core-duplicate-members",
                                &[("count", page.total.to_string().into())]
                            )
                        )?;
                        for member in page.rows {
                            writeln!(
                                output,
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
                            )?;
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
            let mut session = rivu_client::client_session(data_dir);
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
            writeln!(
                output,
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
            )?;
            Ok(())
        }
        action => {
            let (command, wait) = translate(action)?;
            submit_command(i18n, data_dir, command, wait, json)
        }
    };
    output.flush()?;
    result
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

fn translate(action: LibraryAction) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
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
        | LibraryAction::Optimize
        | LibraryAction::Other(_)
        | LibraryAction::Edit { .. }
        | LibraryAction::Stats
        | LibraryAction::History { .. }
        | LibraryAction::Probe { .. }
        | LibraryAction::Tags { .. }
        | LibraryAction::Duplicates { .. }
        | LibraryAction::Relocate { .. } => bail!("This action is not a library command"),
    };
    Ok((command, wait))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8000_u32.to_le_bytes());
        bytes.extend_from_slice(&16000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&0_i16.to_le_bytes());
        bytes
    }

    #[test]
    fn probe_json_accepts_non_utf8_source_names_without_mutating_the_file() -> Result<()> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempfile::tempdir()?;
        let i18n = Localizer::for_cli("en-US");
        for (parent, filename) in [
            (b"parent".to_vec(), b"source\xff.wav".to_vec()),
            (b"parent\xff".to_vec(), b"source.wav".to_vec()),
        ] {
            let parent = directory.path().join(OsString::from_vec(parent));
            fs::create_dir(&parent)?;
            let path = parent.join(OsString::from_vec(filename));
            let original = wav();
            fs::write(&path, &original)?;
            run(
                &i18n,
                &directory.path().join("no-server"),
                &directory.path().join("absent.toml"),
                LibraryAction::Probe {
                    path: path.clone(),
                    enhanced: false,
                },
                true,
            )?;
            assert_eq!(fs::read(path)?, original);
        }
        assert!(!directory.path().join("no-server").exists());
        Ok(())
    }

    #[test]
    fn source_tag_commands_are_offline_and_keep_original_backup() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.wav");
        let original = wav();
        fs::write(&path, &original)?;
        let config = directory.path().join("invalid.toml");
        fs::write(&config, "not valid configuration [")?;
        let data_dir = directory.path().join("no-server");
        let i18n = Localizer::for_cli("en-US");

        for json in [false, true] {
            run(
                &i18n,
                &data_dir,
                &config,
                LibraryAction::Tags {
                    path: path.clone(),
                    title: None,
                    artist: None,
                    album: None,
                },
                json,
            )?;
        }
        assert_eq!(fs::read(&path)?, original);
        assert_eq!(fs::read_dir(directory.path())?.count(), 2);

        for json in [false, true] {
            run(
                &i18n,
                &data_dir,
                &config,
                LibraryAction::Tags {
                    path: path.clone(),
                    title: Some("Source title".into()),
                    artist: Some("Source artist".into()),
                    album: None,
                },
                json,
            )?;
            let tags = rivu_metadata::read_tags(&mut fs::File::open(&path)?)?
                .context("supported WAV tags")?;
            assert_eq!(tags.title.as_deref(), Some("Source title"));
            assert_eq!(tags.artist.as_deref(), Some("Source artist"));
            assert_eq!(fs::read_dir(directory.path())?.count(), 3);
        }
        let backup = fs::read_dir(directory.path())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .find(|candidate| candidate != &path && candidate != &config)
            .context("missing original-file backup")?;
        assert_eq!(fs::read(backup)?, original);
        assert!(
            !data_dir.exists(),
            "offline tag commands must not create a server data directory"
        );
        Ok(())
    }
}
