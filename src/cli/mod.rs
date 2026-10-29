mod args;
mod library;
mod output;
mod processing;
mod tasks;

use self::{
    args::{
        Action, Args, PlaybackAction, PlaylistAction, QueueAction, Repeat, Switch, localize_help,
        terminal_cli_error,
    },
    output::{TerminalEscaped, list_playlists, list_queue, show_ack, show_state},
};
use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches};
use rivu::{app::App, config, i18n::Localizer, terminal};
use rivu_audio as audio;
use rivu_protocol::{
    model::{Command, RepeatMode},
    response::StateSections,
};
use std::path::PathBuf;

pub(super) struct CliError {
    error: anyhow::Error,
    json: bool,
}

pub(super) fn print_error(error: &CliError) {
    let _ = output::write_error(&mut std::io::stderr().lock(), &error.error, error.json);
}

pub(super) fn run() -> std::result::Result<(), CliError> {
    // Command-line surfaces use the process locale, independently of GUI settings.
    let language = "auto";
    let i18n = Localizer::new(language);
    let mut command = Args::command();
    command.build();
    localize_help(&mut command, &i18n, "");
    let matches = command
        .try_get_matches()
        .unwrap_or_else(|error| terminal_cli_error(error).exit());
    let args =
        Args::from_arg_matches(&matches).unwrap_or_else(|error| terminal_cli_error(error).exit());
    args.validate(&matches)
        .unwrap_or_else(|error| terminal_cli_error(error).exit());
    let json = args.json;
    execute(args, i18n).map_err(|error| CliError { error, json })
}

fn execute(args: Args, i18n: Localizer) -> Result<()> {
    let config::paths::Paths {
        config: config_path,
        data_dir,
    } = config::paths::Paths::resolve(args.data_dir, args.config);
    let action = args.action.unwrap_or(Action::Gui { paths: Vec::new() });
    match action {
        Action::Processing(action) => processing::run(&i18n, &data_dir, action, args.json),
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
        Action::Tui => terminal::run(&data_dir, "auto"),
        Action::Library(action) => library::run(&i18n, &data_dir, &config_path, action, args.json),
        Action::Playback(PlaybackAction::Devices) => {
            let devices = audio::devices()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                for device in devices {
                    println!("{}", TerminalEscaped(device));
                }
            }
            Ok(())
        }
        Action::Status => {
            let response = rivu_client::get_state(&data_dir, StateSections::ALL)?;
            show_state(&i18n, &response, args.json)
        }
        Action::Task(action) => tasks::run(&i18n, &data_dir, action, args.json),
        Action::Queue(QueueAction::List) => list_queue(&i18n, &data_dir, args.json),
        Action::Playlist(PlaylistAction::List) => list_playlists(&i18n, &data_dir, args.json),
        Action::Playlist(PlaylistAction::Show { playlist_id }) => {
            output::show_playlist(&i18n, &data_dir, playlist_id, args.json)
        }
        Action::Quit => {
            let ack = rivu_client::request_ack(&data_dir, rivu_protocol::AppCommand::Shutdown)?;
            show_ack(&i18n, &ack, args.json)
        }
        action => {
            let (command, wait_scan) = translate(action)?;
            submit_command(&i18n, &data_dir, command, wait_scan, args.json)
        }
    }
}

fn submit_command(
    i18n: &Localizer,
    data_dir: &std::path::Path,
    command: Command,
    wait_scan: bool,
    json: bool,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir)?;
    let ack = session.request_ack(command)?;
    if !ack.is_success() {
        return output::show_task_ack(i18n, &session, &ack, json);
    }
    if wait_scan {
        let id = ack.scan_id.context("Scan startup omitted task ID")?;
        return tasks::wait_scan(i18n, &mut session, id, json);
    }
    output::show_task_ack(i18n, &session, &ack, json)
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
        Action::Library(action) => return library::translate(action),
        Action::Playback(PlaybackAction::Play { track_id }) => {
            track_id.map_or(Command::Resume, |track_id| Command::Play { track_id })
        }
        Action::Playback(PlaybackAction::Pause) => Command::Pause,
        Action::Playback(PlaybackAction::Toggle) => Command::Toggle,
        Action::Playback(PlaybackAction::Stop) => Command::Stop,
        Action::Playback(PlaybackAction::Next) => Command::Next,
        Action::Playback(PlaybackAction::Previous) => Command::Previous,
        Action::Playback(PlaybackAction::Seek { seconds }) => Command::Seek { seconds },
        Action::Playback(PlaybackAction::Volume { percent }) => Command::Volume {
            value: f32::from(percent) / 100.0,
        },
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
            QueueAction::AddOther => Command::EnqueueSources {
                directories: Vec::new(),
                track_ids: Vec::new(),
                other_tracks: true,
            },
            QueueAction::AddPaths {
                paths,
                index,
                expected_revision,
                wait: should_wait,
            } => {
                wait = should_wait;
                Command::QueueExternalPaths {
                    paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                    index,
                    expected_revision,
                }
            }
            QueueAction::Remove { queue_id } => Command::RemoveQueue { queue_id },
            QueueAction::Move { queue_id, index } => Command::MoveQueue { queue_id, index },
            QueueAction::Play { queue_id } => Command::PlayQueue { queue_id },
            QueueAction::Clear => Command::ClearQueue,
            QueueAction::List => unreachable!(),
        },
        Action::Playlist(action) => match action {
            PlaylistAction::Create { name } => Command::CreatePlaylist { name },
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
            PlaylistAction::List | PlaylistAction::Show { .. } => unreachable!(),
        },
        Action::Playback(PlaybackAction::Device { name }) => Command::Device { name },
        Action::Gui { .. }
        | Action::Serve { .. }
        | Action::Tui
        | Action::Quit
        | Action::Status
        | Action::Task(_)
        | Action::Playback(PlaybackAction::Devices)
        | Action::Processing(_) => bail!("This action is not a playback command"),
    };
    Ok((command, wait))
}

#[cfg(test)]
mod tests {
    use super::args::LibraryAction;
    use super::{Action, Command, PlaylistAction, translate};
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

#[cfg(test)]
mod other_tracks_tests {
    use super::{Action, Args, Command, translate};
    use super::{args::LibraryAction, library::other_tracks_query};
    use anyhow::Result;
    use clap::Parser;
    use rivu_protocol::model::LibrarySort;
    use rivu_protocol::model::Query;

    fn action(arguments: &[&str]) -> Action {
        Args::try_parse_from(std::iter::once("rivu").chain(arguments.iter().copied()))
            .unwrap_or_else(|error| panic!("{error}"))
            .action
            .expect("explicit action")
    }

    fn query(arguments: &[&str]) -> Query {
        let Action::Library(LibraryAction::Other(action)) = action(arguments) else {
            panic!("expected Other action");
        };
        other_tracks_query(action).unwrap()
    }

    #[test]
    fn other_page_preserves_filters_sort_and_bounds() {
        let Query::OtherTracksPage {
            query: text,
            favorite,
            missing,
            sort,
            offset,
            limit,
        } = query(&[
            "library",
            "other",
            "list",
            "--query",
            "two words",
            "--favorite",
            "false",
            "--missing",
            "true",
            "--sort",
            "most-played",
            "--offset",
            "23",
            "--limit",
            "7",
        ])
        else {
            panic!("expected Other page");
        };
        assert_eq!(text.as_deref(), Some("two words"));
        assert_eq!(favorite, Some(false));
        assert_eq!(missing, Some(true));
        assert_eq!(sort, LibrarySort::MostPlayed);
        assert_eq!((offset, limit), (23, 7));

        let Query::OtherTracksPage {
            query: text,
            favorite,
            missing,
            sort,
            offset,
            limit,
        } = query(&["library", "other", "list"])
        else {
            panic!("expected Other page");
        };
        assert_eq!(text, None);
        assert_eq!(favorite, None);
        assert_eq!(missing, None);
        assert_eq!(sort, LibrarySort::Id);
        assert_eq!((offset, limit), (0, 100));
    }

    #[test]
    fn other_locate_uses_the_same_view_contract() {
        let Query::OtherTracksLocate {
            track_id,
            query: text,
            favorite,
            missing,
            sort,
        } = query(&[
            "library",
            "other",
            "locate",
            "42",
            "--query",
            "album",
            "--favorite",
            "true",
            "--missing",
            "false",
            "--sort",
            "album",
        ])
        else {
            panic!("expected Other locate");
        };
        assert_eq!(track_id, 42);
        assert_eq!(text.as_deref(), Some("album"));
        assert_eq!(favorite, Some(true));
        assert_eq!(missing, Some(false));
        assert_eq!(sort, LibrarySort::Album);
        assert!(matches!(
            query(&["library", "other", "count"]),
            Query::OtherTracksCount
        ));
    }

    #[test]
    fn other_checks_reuse_scan_wait_and_cancellation_commands() -> Result<()> {
        let (command, wait) = translate(action(&["library", "other", "check"]))?;
        assert!(matches!(
            command,
            Command::CheckOtherTracks { force: false }
        ));
        assert!(!wait);
        let (command, wait) =
            translate(action(&["library", "other", "check", "--force", "--wait"]))?;
        assert!(matches!(command, Command::CheckOtherTracks { force: true }));
        assert!(wait);
        Ok(())
    }

    #[test]
    fn external_queue_admission_anchors_paths_and_preserves_revision_contract() -> Result<()> {
        let cwd = std::env::current_dir()?;
        let (command, wait) = translate(action(&[
            "queue",
            "add-paths",
            "music/../song.wav",
            "~/disc.cue",
        ]))?;
        let Command::QueueExternalPaths {
            paths,
            index,
            expected_revision,
        } = command
        else {
            panic!("expected external queue admission");
        };
        assert_eq!(
            paths,
            [cwd.join("music/../song.wav"), cwd.join("~/disc.cue")]
        );
        assert_eq!((index, expected_revision), (None, None));
        assert!(!wait);
        let (command, wait) = translate(action(&["queue", "add-paths", "song.wav", "--wait"]))?;
        assert!(matches!(
            command,
            Command::QueueExternalPaths {
                index: None,
                expected_revision: None,
                ..
            }
        ));
        assert!(wait);

        for revision in [0, u64::MAX] {
            let revision_text = revision.to_string();
            let (command, wait) = translate(action(&[
                "queue",
                "add-paths",
                "/music/track.wav",
                "--index",
                "0",
                "--expected-revision",
                &revision_text,
                "--wait",
            ]))?;
            let Command::QueueExternalPaths {
                paths,
                index,
                expected_revision,
            } = command
            else {
                panic!("expected insertion");
            };
            assert_eq!(paths, [std::path::PathBuf::from("/music/track.wav")]);
            assert_eq!((index, expected_revision), (Some(0), Some(revision)));
            assert!(wait);
        }
        Ok(())
    }

    #[test]
    fn queue_other_uses_explicit_source_membership() -> Result<()> {
        let (command, wait) = translate(action(&["queue", "add-other"]))?;
        let Command::EnqueueSources {
            directories,
            track_ids,
            other_tracks,
        } = command
        else {
            panic!("expected Other sources");
        };
        assert!(other_tracks);
        assert!(directories.is_empty());
        assert!(track_ids.is_empty());
        assert!(!wait);
        let (command, _) = translate(action(&["queue", "add", "3", "1"]))?;
        assert!(matches!(command, Command::Enqueue { track_ids } if track_ids == [3, 1]));
        Ok(())
    }

    #[test]
    fn parser_rejects_incomplete_insertions_and_invalid_view_options() {
        for arguments in [
            vec!["queue", "add-paths"],
            vec!["queue", "add-paths", "a.wav", "--index", "0"],
            vec!["queue", "add-paths", "a.wav", "--expected-revision", "1"],
            vec!["library", "other", "list", "--sort", "title"],
            vec!["library", "other", "list", "--favorite", "yes"],
            vec!["library", "other", "count", "--query", "x"],
            vec!["library", "other", "check", "/not-a-root"],
        ] {
            assert!(Args::try_parse_from(std::iter::once("rivu").chain(arguments)).is_err());
        }
    }
}
