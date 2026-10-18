mod args;
mod output;

use self::{
    args::{
        Action, Args, LibraryAction, PlaybackAction, PlaylistAction, QueueAction, Repeat, Switch,
        localize_help, terminal_cli_error,
    },
    output::{
        Terminal, list_library, list_playlists, list_queue, show_ack, show_scan, show_state,
        terminal_text,
    },
};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches};
use rivu::{
    app::App,
    audio, config,
    i18n::Localizer,
    ipc,
    model::{Command, Query, RepeatMode},
    response::{StateSections, ViewResponse},
    terminal,
};
use std::{path::PathBuf, thread, time::Duration};

pub(super) fn print_error(error: &anyhow::Error) {
    eprintln!("rivu: {:#}", Terminal(error));
}

pub(super) fn run() -> Result<()> {
    // Command-line surfaces use the process locale, independently of GUI settings.
    let language = "auto";
    let i18n = Localizer::new(language);
    let mut command = Args::command();
    command.build();
    localize_help(&mut command, &i18n, "");
    let matches = command
        .try_get_matches()
        .unwrap_or_else(|error| terminal_cli_error(error).exit());
    let args = Args::from_arg_matches(&matches)?;
    let config_path = args.config.unwrap_or_else(|| {
        args.data_dir
            .as_ref()
            .map_or_else(config::default_path, |directory| {
                directory.join("config.toml")
            })
    });
    let data_dir = args.data_dir.unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "rivu")
            .map(|dirs| dirs.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".rivu"))
    });
    let action = args.action.unwrap_or(Action::Gui { paths: Vec::new() });
    match action {
        Action::Gui { paths } => App::open_gui(
            data_dir,
            config_path,
            paths.into_iter().map(client_path).collect::<Result<_>>()?,
        ),
        Action::Serve { paths } => App::start(
            data_dir,
            config_path,
            paths.into_iter().map(client_path).collect::<Result<_>>()?,
            false,
        )?
        .run(),
        Action::Tui => terminal::run(&data_dir, language),
        Action::Library(LibraryAction::Probe { path, enhanced }) => {
            let probe_config = config::Config::load(&config_path)?;
            let ffmpeg_enabled = probe_config.ffmpeg_enabled || enhanced;
            let result = audio::probe_details(&path, ffmpeg_enabled, enhanced)?;
            let info = result.info;
            let decoder_status = if ffmpeg_enabled {
                audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
            } else {
                "disabled".to_owned()
            };
            if args.json {
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
                        ("path", Terminal(path.display()).to_string().into()),
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
        Action::Playback(PlaybackAction::Devices) => {
            let devices = audio::devices()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                for device in devices {
                    println!("{}", Terminal(device));
                }
            }
            Ok(())
        }
        Action::Library(LibraryAction::Status) => {
            let response = ipc::get_state(&data_dir, StateSections::ALL)?;
            show_state(&i18n, &response, args.json)
        }
        Action::Library(LibraryAction::List {
            query,
            favorites,
            missing,
        }) => list_library(&i18n, &data_dir, args.json, query, favorites, missing),
        Action::Library(LibraryAction::Stats) => {
            let response = ipc::query(&data_dir, &Query::LibraryStats)?;
            let ViewResponse::LibraryStats(stats) = response.result.map_err(anyhow::Error::from)?
            else {
                bail!("Statistics query returned an unexpected response");
            };
            if args.json {
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
        Action::Library(LibraryAction::History { limit }) => {
            let response = ipc::get_state(
                &data_dir,
                StateSections {
                    library: true,
                    ..StateSections::default()
                },
            )?;
            if args.json {
                return show_state(&i18n, &response, true);
            }
            let state = response
                .library
                .as_ref()
                .context("State omitted library section")?;
            for entry in state.history.iter().take(limit) {
                println!(
                    "{}\t{}\t{}",
                    entry.played_at,
                    entry.track_id,
                    Terminal(&entry.title)
                );
            }
            Ok(())
        }
        Action::Queue(QueueAction::List) => list_queue(&i18n, &data_dir, args.json),
        Action::Playlist(PlaylistAction::List) => list_playlists(&i18n, &data_dir, args.json),
        Action::Library(LibraryAction::Edit {
            track_id,
            title,
            artist,
            album,
        }) => {
            let command = Command::EditTrack {
                track_id,
                title,
                artist,
                album,
                decoder_hint: None,
            };
            let ack = ipc::request_ack(&data_dir, &command)?;
            show_ack(&i18n, &ack, args.json)
        }
        Action::Library(LibraryAction::Duplicates {
            fingerprint,
            offset,
            limit,
        }) => {
            let query = match fingerprint {
                Some(fingerprint) => Query::DuplicateMembers {
                    fingerprint,
                    offset,
                    limit,
                },
                None => Query::DuplicateGroups { offset, limit },
            };
            let view = ipc::query(&data_dir, &query)?
                .result
                .map_err(anyhow::Error::from)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&view)?);
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
                                            Terminal(member.track.path.display())
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
        Action::Library(LibraryAction::RelocationStatus { id }) => {
            let view = ipc::query(&data_dir, &Query::RelocationTask { id })?
                .result
                .map_err(anyhow::Error::from)?;
            let ViewResponse::RelocationTask(task) = view else {
                bail!("Unexpected relocation status response");
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                let phase = i18n.text(match task.phase {
                    rivu::model::RelocationPhase::Verifying => "core-verifying",
                    rivu::model::RelocationPhase::Completed => "core-completed",
                    rivu::model::RelocationPhase::Cancelled => "core-cancelled",
                    rivu::model::RelocationPhase::Failed => "core-failed",
                });
                println!(
                    "{}",
                    i18n.format(
                        "core-relocation-status",
                        &[
                            ("id", task.id.to_string().into()),
                            ("source", task.source_id.to_string().into()),
                            ("target", task.target_id.to_string().into()),
                            ("phase", phase.into()),
                        ]
                    )
                );
                if let Some(error) = task.error {
                    eprintln!("{}", Terminal(error));
                }
            }
            Ok(())
        }
        Action::Library(LibraryAction::Relocate {
            source_id,
            target_id,
            wait,
        }) => {
            let mut session = ipc::client_session(&data_dir)?;
            let ack = session.request_ack(&Command::RelocateTrack {
                source_id,
                target_id,
            })?;
            if !ack.ok {
                bail!("{}", ack.error.as_deref().unwrap_or("Relocation refused"));
            }
            let id = ack
                .relocation_id
                .context("Relocation startup omitted task ID")?;
            if !wait {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&ack)?);
                } else {
                    println!(
                        "{}",
                        i18n.format("core-relocation-accepted", &[("id", id.to_string().into())])
                    );
                }
                return Ok(());
            }
            loop {
                thread::sleep(Duration::from_millis(200));
                let view = session
                    .query(&Query::RelocationTask { id })?
                    .result
                    .map_err(anyhow::Error::from)?;
                let ViewResponse::RelocationTask(task) = view else {
                    bail!("Unexpected relocation status response");
                };
                match task.phase {
                    rivu::model::RelocationPhase::Verifying => continue,
                    rivu::model::RelocationPhase::Completed => {
                        if args.json {
                            println!("{}", serde_json::to_string_pretty(&task)?);
                        } else {
                            println!(
                                "{}",
                                i18n.format(
                                    "core-relocation-completed",
                                    &[
                                        ("source", source_id.to_string().into()),
                                        ("target", target_id.to_string().into())
                                    ]
                                )
                            );
                            if let Some(error) = task.error {
                                eprintln!("{}", Terminal(error));
                            }
                        }
                        return Ok(());
                    }
                    _ => bail!(
                        "{}",
                        task.error.as_deref().unwrap_or("Relocation cancelled")
                    ),
                }
            }
        }
        Action::Library(LibraryAction::Optimize) => {
            let ack = ipc::request_ack(&data_dir, &Command::OptimizeDatabase)?;
            if !ack.ok {
                return show_ack(&i18n, &ack, args.json);
            }
            let response = ipc::get_state(
                &data_dir,
                StateSections {
                    system: true,
                    ..StateSections::default()
                },
            )?;
            if args.json {
                return show_state(&i18n, &response, true);
            }
            let report = response
                .system
                .as_ref()
                .and_then(|state| state.database_optimization.as_ref())
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
        Action::Quit => {
            let ack = ipc::request_ack(&data_dir, &rivu::app::AppCommand::Shutdown)?;
            show_ack(&i18n, &ack, args.json)
        }
        action => {
            let (command, wait_scan) = translate(action)?;
            let mut session = ipc::client_session(&data_dir)?;
            let ack = session.request_ack(&command)?;
            if !ack.ok {
                bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
            }
            if wait_scan {
                let id = ack.scan_id.context("Scan startup omitted task ID")?;
                loop {
                    thread::sleep(Duration::from_millis(200));
                    let response = session.query(&Query::ScanTask { id })?;
                    let ViewResponse::ScanTask(task) =
                        response.result.map_err(anyhow::Error::from)?
                    else {
                        bail!("Scan task query returned an unexpected response");
                    };
                    if task.id != id {
                        bail!("Scan task query returned a different task ID");
                    }
                    match task.progress.phase {
                        rivu::model::ScanPhase::Completed => {
                            if args.json {
                                println!("{}", serde_json::to_string_pretty(&task)?);
                            } else {
                                show_scan(&i18n, &task.progress);
                            }
                            return Ok(());
                        }
                        rivu::model::ScanPhase::Failed | rivu::model::ScanPhase::Cancelled => {
                            bail!("{}", task.error.as_deref().unwrap_or(&task.message));
                        }
                        _ => {}
                    }
                }
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&ack)?);
            } else {
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
    }
}

// Resolve against the caller's cwd, without expanding or canonicalizing input.
// In particular, playlist export destinations need not exist yet.
fn client_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("resolve client working directory")?
            .join(path))
    }
}

fn translate(action: Action) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
        Action::Library(LibraryAction::Optimize) => Command::OptimizeDatabase,
        Action::Library(LibraryAction::Scan {
            paths,
            wait: should_wait,
            force,
        }) => {
            wait = should_wait;
            Command::Scan {
                paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                force,
            }
        }
        Action::Playback(PlaybackAction::Play { track_id }) => {
            track_id.map_or(Command::Resume, |track_id| Command::Play { track_id })
        }
        Action::Playback(PlaybackAction::Pause) => Command::Pause,
        Action::Playback(PlaybackAction::Toggle) => Command::Toggle,
        Action::Playback(PlaybackAction::Stop) => Command::Stop,
        Action::Playback(PlaybackAction::Next) => Command::Next,
        Action::Playback(PlaybackAction::Previous) => Command::Previous,
        Action::Playback(PlaybackAction::Seek { seconds }) => Command::Seek { seconds },
        Action::Playback(PlaybackAction::Volume { percent }) => {
            if percent > 100 {
                bail!("Volume must be an integer between 0 and 100");
            }
            Command::Volume {
                value: f32::from(percent) / 100.0,
            }
        }
        Action::Playback(PlaybackAction::Shuffle { mode }) => Command::Shuffle {
            enabled: matches!(mode, Switch::On),
        },
        Action::Playback(PlaybackAction::Repeat { mode }) => Command::Repeat {
            mode: match mode {
                Repeat::Off => RepeatMode::Off,
                Repeat::All => RepeatMode::All,
                Repeat::One => RepeatMode::One,
            },
        },
        Action::Queue(action) => match action {
            QueueAction::Add { track_ids } => Command::Enqueue { track_ids },
            QueueAction::Remove { queue_id } => Command::RemoveQueue { queue_id },
            QueueAction::Move { queue_id, index } => Command::MoveQueue { queue_id, index },
            QueueAction::Play { queue_id } => Command::PlayQueue { queue_id },
            QueueAction::Clear => Command::ClearQueue,
            QueueAction::List => unreachable!(),
        },
        Action::Playlist(action) => match action {
            PlaylistAction::New { name } => Command::CreatePlaylist { name },
            PlaylistAction::Rename { playlist_id, name } => {
                Command::RenamePlaylist { playlist_id, name }
            }
            PlaylistAction::Delete { playlist_id } => Command::DeletePlaylist { playlist_id },
            PlaylistAction::Add {
                playlist_id,
                track_ids,
            } => Command::AddPlaylist {
                playlist_id,
                track_ids,
            },
            PlaylistAction::Remove { entry_id } => Command::RemovePlaylistEntry { entry_id },
            PlaylistAction::Move { entry_id, index } => {
                Command::MovePlaylistEntry { entry_id, index }
            }
            PlaylistAction::Play { playlist_id } => Command::PlayPlaylist { playlist_id },
            PlaylistAction::Import {
                path,
                name,
                wait: should_wait,
            } => {
                wait = should_wait;
                Command::ImportPlaylist {
                    path: client_path(path)?,
                    name,
                }
            }
            PlaylistAction::Export { playlist_id, path } => Command::ExportPlaylist {
                playlist_id,
                path: client_path(path)?,
            },
            PlaylistAction::List => unreachable!(),
        },
        Action::Library(LibraryAction::Remove { track_ids }) => Command::RemoveTracks { track_ids },
        Action::Library(LibraryAction::Favorite { track_ids, set }) => Command::SetFavorite {
            track_ids,
            favorite: matches!(set, Switch::On),
        },
        Action::Library(LibraryAction::CleanMissing) => Command::RemoveMissingTracks,
        Action::Library(LibraryAction::CancelRelocation { id }) => Command::CancelRelocation { id },
        Action::Library(LibraryAction::KeepIndependent { track_id }) => {
            Command::KeepTrackIndependent { track_id }
        }
        Action::Playback(PlaybackAction::Device { name }) => Command::Device { name },
        Action::Gui { .. }
        | Action::Serve { .. }
        | Action::Tui
        | Action::Quit
        | Action::Playback(PlaybackAction::Devices)
        | Action::Library(
            LibraryAction::List { .. }
            | LibraryAction::Edit { .. }
            | LibraryAction::Status
            | LibraryAction::Stats
            | LibraryAction::History { .. }
            | LibraryAction::Probe { .. }
            | LibraryAction::Duplicates { .. }
            | LibraryAction::Relocate { .. }
            | LibraryAction::RelocationStatus { .. },
        ) => bail!("This action is not a playback command"),
    };
    Ok((command, wait))
}

#[cfg(test)]
mod tests {
    use super::{Action, Command, LibraryAction, PlaylistAction, translate};
    use anyhow::Result;

    #[test]
    fn scan_paths_are_anchored_without_expansion() -> Result<()> {
        let cwd = std::env::current_dir()?;
        let absolute = cwd.join("absolute.wav");
        let (command, wait) = translate(Action::Library(LibraryAction::Scan {
            paths: vec![
                "music/../song.wav".into(),
                "~/song.wav".into(),
                absolute.clone(),
            ],
            wait: true,
            force: true,
        }))?;
        let Command::Scan { paths, force } = command else {
            panic!("expected scan command");
        };
        assert!(wait);
        assert!(force);
        assert_eq!(
            paths,
            [
                cwd.join("music/../song.wav"),
                cwd.join("~/song.wav"),
                absolute
            ]
        );
        Ok(())
    }

    #[test]
    fn playlist_paths_are_anchored_without_requiring_destination() -> Result<()> {
        let cwd = std::env::current_dir()?;
        let directory = tempfile::tempdir_in(&cwd)?;
        let relative = directory.path().strip_prefix(&cwd)?.join("new.m3u8");
        assert!(!relative.exists());
        let (command, wait) = translate(Action::Playlist(PlaylistAction::Export {
            playlist_id: 7,
            path: relative.clone(),
        }))?;
        let Command::ExportPlaylist { playlist_id, path } = command else {
            panic!("expected export command");
        };
        assert_eq!(playlist_id, 7);
        assert_eq!(path, cwd.join(&relative));
        assert!(!wait);
        let (command, wait) = translate(Action::Playlist(PlaylistAction::Import {
            path: relative.clone(),
            name: Some("Imported".into()),
            wait: true,
        }))?;
        let Command::ImportPlaylist { path, name } = command else {
            panic!("expected import command");
        };
        assert_eq!(path, cwd.join(relative));
        assert_eq!(name.as_deref(), Some("Imported"));
        assert!(wait);
        Ok(())
    }
}
