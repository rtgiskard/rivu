mod args;
mod library;
mod output;
mod processing;
mod processing_setup;
mod tasks;

use self::{
    args::{
        Action, Args, PlaybackAction, PlaylistAction, QueueAction, Repeat, Switch, localize_help,
    },
    output::{list_playlists, list_queue, show_ack, show_state},
};
use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};
use rivu::{app::App, config};
use rivu_audio as audio;
use rivu_presentation::i18n::Localizer;
use rivu_protocol::{
    model::{Command, RepeatMode},
    response::StateSections,
};
use rivu_utils::terminal::{TerminalEscaped, cli_error};
use std::{io::Write, path::PathBuf};

pub(super) struct CliError {
    error: anyhow::Error,
    json: bool,
}

pub(super) fn print_error(error: &CliError) {
    let mut output = std::io::stderr().lock();
    if let Err(output_error) = output::write_error(&mut output, &error.error, error.json) {
        let fallback = anyhow::anyhow!(
            "{:#}; could not write the requested error output: {output_error:#}",
            error.error
        );
        let _ = output::write_error(&mut output, &fallback, false);
    }
}

pub(super) fn run() -> std::result::Result<(), CliError> {
    // Command-line surfaces use the process locale, independently of GUI settings.
    let i18n = Localizer::for_cli("auto");
    let mut command = Args::command();
    localize_help(&mut command, &i18n, "");
    let matches = match command.try_get_matches_from_mut(std::env::args_os()) {
        Ok(matches) => matches,
        Err(error) => return finish_parse(error),
    };
    let args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(error) => return finish_parse(error),
    };
    if let Err(error) = args.validate(&matches) {
        return finish_parse(error);
    }
    if args.action.is_none() {
        command.print_help().map_err(|error| CliError {
            error: error.into(),
            json: args.json,
        })?;
        std::io::stdout().lock().flush().map_err(|error| CliError {
            error: error.into(),
            json: args.json,
        })?;
        return Ok(());
    }
    let json = args.json;
    execute(args, i18n).map_err(|error| CliError { error, json })
}

fn finish_parse(error: clap::Error) -> std::result::Result<(), CliError> {
    let error = cli_error(error);
    let result = error.print().and_then(|()| {
        if error.use_stderr() {
            std::io::stderr().lock().flush()
        } else {
            std::io::stdout().lock().flush()
        }
    });
    result.map_err(|error| CliError {
        error: error.into(),
        json: false,
    })?;
    if error.exit_code() == 0 {
        Ok(())
    } else {
        std::process::exit(error.exit_code());
    }
}

fn execute(args: Args, i18n: Localizer) -> Result<()> {
    rivu_utils::logging::init(
        args.log_level,
        if args.json {
            rivu_utils::logging::StderrFormat::Json
        } else {
            rivu_utils::logging::StderrFormat::Text
        },
    )?;
    let config::paths::Paths {
        config: config_path,
        data_dir,
    } = config::paths::Paths::resolve(args.data_dir, args.config);
    let action = args.action.expect("run requires an explicit subcommand");
    let mut wait_scan = false;
    let command = match action {
        Action::Processing(action) => return processing::run(&i18n, &data_dir, action, args.json),
        #[cfg(feature = "gui")]
        Action::Gui { paths } => {
            return App::open_gui(
                data_dir,
                config_path,
                paths.into_iter().map(client_path).collect::<Result<_>>()?,
            );
        }
        Action::Serve { paths } => {
            return App::start(
                data_dir,
                config_path,
                paths.into_iter().map(client_path).collect::<Result<_>>()?,
            )?
            .run();
        }
        #[cfg(feature = "tui")]
        Action::Tui => return App::open_tui(data_dir, config_path),
        Action::Library(action) => {
            return library::run(&i18n, &data_dir, &config_path, action, args.json);
        }
        Action::Playback(PlaybackAction::Devices) => {
            let devices = audio::devices()?;
            let mut output = std::io::stdout().lock();
            if args.json {
                writeln!(output, "{}", serde_json::to_string_pretty(&devices)?)?;
            } else {
                for device in devices {
                    writeln!(output, "{}", TerminalEscaped(device))?;
                }
            }
            output.flush()?;
            return Ok(());
        }
        Action::Status => {
            let sections = if args.json {
                StateSections::ALL
            } else {
                StateSections {
                    playback: true,
                    queue: true,
                    library: true,
                    system: true,
                    ..Default::default()
                }
            };
            let response = rivu_client::get_state(&data_dir, sections)?;
            return show_state(&i18n, &response, args.json);
        }
        Action::Task(action) => return tasks::run(&i18n, &data_dir, action, args.json),
        Action::Quit => {
            let ack = rivu_client::request_ack(&data_dir, rivu_protocol::AppCommand::Shutdown)?;
            return show_ack(&i18n, &ack, args.json);
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
                wait_scan = should_wait;
                Command::QueueExternalPaths {
                    paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                    index,
                    expected_revision,
                }
            }
            QueueAction::Remove { queue_id } => Command::RemoveQueue { queue_id },
            QueueAction::Move {
                queue_id,
                index,
                expected_revision,
            } => Command::MoveQueue {
                queue_id,
                index,
                expected_revision,
            },
            QueueAction::Play { queue_id } => Command::PlayQueue { queue_id },
            QueueAction::Clear => Command::ClearQueue,
            QueueAction::List => return list_queue(&i18n, &data_dir, args.json),
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
                wait_scan = should_wait;
                Command::ImportPlaylist {
                    path: client_path(path)?,
                    name,
                }
            }
            PlaylistAction::Export { playlist_id, path } => Command::ExportPlaylist {
                playlist_id,
                path: client_path(path)?,
            },
            PlaylistAction::List => return list_playlists(&i18n, &data_dir, args.json),
            PlaylistAction::Show { playlist_id } => {
                return output::show_playlist(&i18n, &data_dir, playlist_id, args.json);
            }
        },
        Action::Playback(PlaybackAction::Device { name }) => Command::Device { name },
    };
    submit_command(&i18n, &data_dir, command, wait_scan, args.json)
}

fn submit_command(
    i18n: &Localizer,
    data_dir: &std::path::Path,
    command: Command,
    wait_scan: bool,
    json: bool,
) -> Result<()> {
    let mut session = rivu_client::client_session(data_dir);
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

#[cfg(test)]
mod tests {
    use super::client_path;
    use anyhow::Result;

    #[test]
    fn client_paths_are_anchored_without_expansion_or_requiring_destination() -> Result<()> {
        let cwd = std::env::current_dir()?;
        let directory = tempfile::tempdir_in(&cwd)?;
        let relative = directory.path().strip_prefix(&cwd)?.join("new.m3u8");
        assert!(!relative.exists());
        assert_eq!(client_path(relative.clone())?, cwd.join(relative));
        assert_eq!(client_path("~/disc.cue".into())?, cwd.join("~/disc.cue"));
        assert_eq!(
            client_path("music/../song.wav".into())?,
            cwd.join("music/../song.wav")
        );
        let absolute = directory.path().join("absent.wav");
        assert_eq!(client_path(absolute.clone())?, absolute);
        Ok(())
    }
}

#[cfg(test)]
mod other_tracks_tests {
    use super::Args;
    use clap::Parser;

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
