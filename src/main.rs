use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use rivu::{
    audio, config,
    core::Runtime,
    gui, ipc,
    model::{Command, PlaybackStatus, RepeatMode, Response},
    mpris, terminal,
};
use std::{path::PathBuf, thread, time::Duration};

#[derive(Parser)]
#[command(
    name = "rivu",
    version,
    about = "A quiet, local-first music player",
    long_about = "Rivu plays local audio with a shared Rust core. Run without a subcommand for the desktop UI; CLI and TUI control that same instance. Optional FFmpeg extension audio decoding is loaded only when enabled."
)]
struct Args {
    #[arg(long, global = true, value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Open the desktop UI and optionally import paths.
    Gui { paths: Vec<PathBuf> },
    /// Run the shared core without a GUI (foreground).
    Serve { paths: Vec<PathBuf> },
    /// Control the running instance in a minimal terminal UI.
    Tui,
    /// Control playback, volume, and audio output devices.
    #[command(subcommand)]
    Playback(PlaybackAction),
    /// Import and manage tracks, inspect metadata, and view listening information.
    #[command(subcommand)]
    Library(LibraryAction),
    /// Manage the current playback queue.
    #[command(subcommand)]
    Queue(QueueAction),
    /// Manage, import, and export playlists.
    #[command(subcommand)]
    Playlist(PlaylistAction),
    /// Shut down the running GUI/headless core.
    Quit,
}

#[derive(Subcommand)]
enum PlaybackAction {
    /// Play a library track ID, or resume the current queue.
    Play {
        track_id: Option<i64>,
    },
    Pause,
    Toggle,
    Stop,
    Next,
    #[command(alias = "prev")]
    Previous,
    Seek {
        seconds: f64,
    },
    /// Set volume as a percentage, 0..100.
    Volume {
        percent: f32,
    },
    Shuffle {
        #[arg(value_enum)]
        mode: Switch,
    },
    Repeat {
        #[arg(value_enum)]
        mode: Repeat,
    },
    /// List available output devices without starting an instance.
    Devices,
    /// Select an output device by exact name, or use the default.
    Device {
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum LibraryAction {
    /// Import audio files, CUE sheets or directories without blocking playback.
    Scan {
        /// Paths relative to this client's working directory, or absolute paths.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        wait: bool,
        /// Reprobe metadata even when file size and modification time are unchanged.
        #[arg(long)]
        force: bool,
    },
    /// List the library, optionally filtered by text.
    List {
        #[arg(short, long)]
        query: Option<String>,
        #[arg(long)]
        favorites: bool,
        #[arg(long)]
        missing: bool,
    },
    /// Edit Rivu's library metadata; does not rewrite media files.
    Edit {
        track_id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long)]
        album: Option<String>,
    },
    /// Remove library records, never the media files.
    Remove {
        #[arg(required = true)]
        track_ids: Vec<i64>,
    },
    /// Set or clear favorites without modifying media files.
    Favorite {
        #[arg(required = true)]
        track_ids: Vec<i64>,
        #[arg(long, value_enum)]
        set: Switch,
    },
    /// Remove missing library records and their references, never media files.
    CleanMissing,
    /// Show the running instance's playback and library state.
    Status,
    /// Show library size and aggregate play counts.
    Stats,
    /// Show recently started tracks, once per track.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read real metadata and check decoder support without starting an instance.
    Probe { path: PathBuf },
    /// Reclaim database space and refresh query statistics; stop playback and scans first.
    Optimize,
}
#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}
#[derive(Clone, Copy, ValueEnum)]
enum Repeat {
    Off,
    All,
    One,
}
#[derive(Subcommand)]
enum QueueAction {
    List,
    Add {
        #[arg(required = true)]
        track_ids: Vec<i64>,
    },
    Remove {
        queue_id: u64,
    },
    Move {
        queue_id: u64,
        index: usize,
    },
    Play {
        queue_id: u64,
    },
    Clear,
}
#[derive(Subcommand)]
enum PlaylistAction {
    List,
    New {
        name: String,
    },
    Rename {
        playlist_id: i64,
        name: String,
    },
    Delete {
        playlist_id: i64,
    },
    Add {
        playlist_id: i64,
        #[arg(required = true)]
        track_ids: Vec<i64>,
    },
    Remove {
        entry_id: i64,
    },
    Move {
        entry_id: i64,
        index: usize,
    },
    Play {
        playlist_id: i64,
    },
    /// Import an M3U/M3U8 playlist or a CUE sheet in track order.
    Import {
        /// Playlist path relative to this client's working directory, or absolute.
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        wait: bool,
    },
    Export {
        playlist_id: i64,
        /// Destination relative to this client's working directory; need not exist.
        path: PathBuf,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rivu: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args = Args::parse();
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
    let socket = data_dir.join("rivu.sock");
    let action = args.action.unwrap_or(Action::Gui { paths: Vec::new() });
    match action {
        Action::Gui { paths } => start(data_dir, config_path, paths, true),
        Action::Serve { paths } => start(data_dir, config_path, paths, false),
        Action::Tui => terminal::run(&socket),
        Action::Library(LibraryAction::Probe { path }) => {
            let probe_config = config::Config::load(&config_path)?;
            let ffmpeg_enabled = probe_config.ffmpeg_enabled;
            let decoder_status = if ffmpeg_enabled {
                audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"))
            } else {
                "disabled".to_owned()
            };
            let info = audio::probe(&path, ffmpeg_enabled)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": path, "title": info.title, "artist": info.artist, "album": info.album,
                        "codec": info.codec, "channels": info.channels, "sample_rate": info.sample_rate,
                        "duration": info.duration, "ffmpeg_enabled": ffmpeg_enabled,
                        "bitrate_bps": info.bitrate_bps, "bits_per_sample": info.bits_per_sample,
                        "track_number": info.track_number, "disc_number": info.disc_number,
                        "release_date": info.release_date,
                        "ffmpeg_status": decoder_status,
                    }))?
                );
                return Ok(());
            }
            println!(
                "{}\n  title: {}\n  artist: {}\n  album: {}\n  codec: {}\n  channels: {}\n  sample rate: {} Hz\n  duration: {}\n  bitrate: {}\n  source bits/sample: {}\n  track number: {}\n  disc number: {}\n  release date: {}\n  FFmpeg extension audio decoding: {}",
                path.display(),
                info.title,
                info.artist,
                info.album,
                info.codec,
                info.channels,
                info.sample_rate,
                info.duration
                    .map_or("unknown".into(), |value| format!("{value:.3} s")),
                info.bitrate_bps.map_or("—".into(), |value| format!(
                    "{:.1} kbps",
                    value as f64 / 1000.0
                )),
                info.bits_per_sample
                    .map_or("—".into(), |value| value.to_string()),
                info.track_number
                    .map_or("—".into(), |value| value.to_string()),
                info.disc_number
                    .map_or("—".into(), |value| value.to_string()),
                info.release_date.as_deref().unwrap_or("—"),
                decoder_status,
            );
            Ok(())
        }
        Action::Playback(PlaybackAction::Devices) => {
            let devices = audio::devices()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                for device in devices {
                    println!("{device}");
                }
            }
            Ok(())
        }
        Action::Library(LibraryAction::Status) => {
            show(&ipc::request(&socket, &Command::Status)?, args.json)
        }
        Action::Library(LibraryAction::List {
            query,
            favorites,
            missing,
        }) => {
            let mut response = checked(ipc::request(&socket, &Command::Status)?)?;
            let query = query.unwrap_or_default().to_lowercase();
            let tracks = response.state.library.iter().filter(|track| {
                (!favorites || track.favorite)
                    && (!missing || track.missing)
                    && (query.is_empty()
                        || format!(
                            "{} {} {} {}",
                            track.title,
                            track.artist,
                            track.album,
                            track.path.display()
                        )
                        .to_lowercase()
                        .contains(&query))
            });
            if args.json {
                let library = tracks.cloned().collect();
                response.state.library = std::sync::Arc::new(library);
                return show(&response, true);
            }
            for track in tracks {
                println!(
                    "{}\t{}\t{}\t{}\t{}{}{}\t{}",
                    track.id,
                    track.title,
                    track.artist,
                    track.album,
                    track.path.display(),
                    if track.missing { " [missing]" } else { "" },
                    if track.favorite { " [favorite]" } else { "" },
                    track.bitrate_bps.map_or("—".into(), |value| format!(
                        "{:.1} kbps",
                        value as f64 / 1000.0
                    ))
                );
            }
            Ok(())
        }
        Action::Library(LibraryAction::Stats) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            println!(
                "Tracks: {}\nPlays: {}",
                response.state.library.len(),
                response
                    .state
                    .library
                    .iter()
                    .map(|t| t.play_count)
                    .sum::<u64>()
            );
            Ok(())
        }
        Action::Library(LibraryAction::History { limit }) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            for entry in response.state.history.iter().take(limit) {
                println!("{}\t{}\t{}", entry.played_at, entry.track_id, entry.title);
            }
            Ok(())
        }
        Action::Queue(QueueAction::List) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            for (index, entry) in response.state.queue.iter().enumerate() {
                let title = response
                    .state
                    .library
                    .iter()
                    .find(|t| t.id == entry.track_id)
                    .map_or("Missing track", |t| t.title.as_str());
                println!(
                    "{}\t{}\t{}\t{}{}",
                    index,
                    entry.id,
                    entry.track_id,
                    title,
                    if response.state.current_queue_id == Some(entry.id) {
                        " *"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        Action::Playlist(PlaylistAction::List) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            for playlist in response.state.playlists.iter() {
                println!(
                    "{}\t{}\t{} entries",
                    playlist.id,
                    playlist.name,
                    playlist.entries.len()
                );
                for (index, entry) in playlist.entries.iter().enumerate() {
                    println!("  {index}\tentry {}\ttrack {}", entry.id, entry.track_id);
                }
            }
            Ok(())
        }
        Action::Library(LibraryAction::Edit {
            track_id,
            title,
            artist,
            album,
        }) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            let track = response
                .state
                .library
                .iter()
                .find(|track| track.id == track_id)
                .context("Track not found")?;
            let command = Command::EditTrack {
                track_id,
                title: title.unwrap_or_else(|| track.title.clone()),
                artist: artist.unwrap_or_else(|| track.artist.clone()),
                album: album.unwrap_or_else(|| track.album.clone()),
            };
            show(&ipc::request(&socket, &command)?, args.json)
        }
        Action::Library(LibraryAction::Optimize) => {
            let response = checked(ipc::request(&socket, &Command::OptimizeDatabase)?)?;
            if args.json {
                return show(&response, true);
            }
            let report = response
                .state
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
                "Database optimization completed\nDatabase: {} -> {} bytes\nWAL: {} -> {} bytes\nReclaimed total: {} bytes",
                report.database_bytes_before,
                report.database_bytes_after,
                report.wal_bytes_before,
                report.wal_bytes_after,
                before.saturating_sub(after)
            );
            Ok(())
        }
        action => {
            let (command, wait_scan) = translate(action)?;
            let response = ipc::request(&socket, &command)?;
            checked(response.clone())?;
            if wait_scan {
                loop {
                    thread::sleep(Duration::from_millis(200));
                    let response = checked(ipc::request(&socket, &Command::Status)?)?;
                    if !response.state.scanning {
                        if let Some(error) = &response.state.last_error {
                            bail!("{error}");
                        }
                        return show(&response, args.json);
                    }
                }
            }
            show(&response, args.json)
        }
    }
}

fn start(
    data_dir: PathBuf,
    config_path: PathBuf,
    paths: Vec<PathBuf>,
    desktop: bool,
) -> Result<()> {
    let socket = data_dir.join("rivu.sock");
    let listener = ipc::bind(&socket)?;
    let mut runtime = match Runtime::start(&data_dir, &config_path) {
        Ok(runtime) => runtime,
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(&socket);
            return Err(error);
        }
    };
    let server = ipc::Server::start(socket, listener, runtime.handle.clone())?;
    let media_handle = runtime.handle.clone();
    let media_changes = media_handle.subscribe();
    let media_data_dir = data_dir.clone();
    let media_worker = thread::Builder::new()
        .name("rivu-mpris-manager".into())
        .spawn(move || {
            let mut service = None;
            let mut enabled = false;
            loop {
                let snapshot = media_handle.snapshot();
                if snapshot.shutting_down {
                    break;
                }
                if snapshot.config.mpris_enabled != enabled {
                    enabled = snapshot.config.mpris_enabled;
                    let status = if enabled {
                        match mpris::Mpris::start(media_handle.clone(), desktop, &media_data_dir) {
                            Ok(started) => {
                                service = Some(started);
                                "Connected to desktop media controls".to_owned()
                            }
                            Err(error) => format!("MPRIS unavailable: {error:#}"),
                        }
                    } else {
                        service = None;
                        "Desktop media controls disabled".to_owned()
                    };
                    let _ = media_handle.send(Command::MprisStatus { status });
                }
                if media_changes.recv().is_err() {
                    break;
                }
            }
            drop(service);
        })?;
    let paths = if paths.is_empty() {
        runtime.handle.snapshot().config.library_roots.clone()
    } else {
        paths
    };
    if !paths.is_empty() {
        runtime.handle.send(Command::Scan {
            paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
            force: false,
        })?;
    }
    if desktop {
        gui::run(runtime.handle.clone(), data_dir.join("workspace.json"))?;
    } else {
        println!("Rivu core ready. Data: {}", data_dir.display());
    }
    runtime.join();
    let _ = media_worker.join();
    drop(server);
    Ok(())
}
fn checked(response: Response) -> Result<Response> {
    if !response.ok {
        bail!("{}", response.error.as_deref().unwrap_or("Command failed"));
    }
    Ok(response)
}
fn show(response: &Response, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
    } else {
        let state = &response.state;
        let status = match state.status {
            PlaybackStatus::Stopped => "stopped",
            PlaybackStatus::Playing => "playing",
            PlaybackStatus::Paused => "paused",
        };
        if let Some(track) = state.current_track() {
            println!(
                "{status}: {} — {}  {:.1}/{} s  volume {:.0}%",
                track.artist,
                track.title,
                state.position,
                state
                    .duration
                    .map_or("?".into(), |value| format!("{value:.1}")),
                state.volume * 100.0
            );
        } else {
            println!(
                "{status} · {} tracks · {} queued",
                state.library.len(),
                state.queue.len()
            );
        }
        if !state.scan_message.is_empty() {
            println!("{}", state.scan_message);
        }
        println!("FFmpeg extension audio decoding: {}", state.ffmpeg_status);
        if let Some(error) = &state.last_error {
            eprintln!("{error}");
        }
    }
    if !response.ok {
        bail!("{}", response.error.as_deref().unwrap_or("Command failed"));
    }
    Ok(())
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
            if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
                bail!("Volume must be between 0 and 100");
            }
            Command::Volume {
                value: percent / 100.0,
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
        Action::Playback(PlaybackAction::Device { name }) => Command::Device { name },
        Action::Quit => Command::Shutdown,
        Action::Gui { .. }
        | Action::Serve { .. }
        | Action::Tui
        | Action::Playback(PlaybackAction::Devices)
        | Action::Library(
            LibraryAction::List { .. }
            | LibraryAction::Edit { .. }
            | LibraryAction::Status
            | LibraryAction::Stats
            | LibraryAction::History { .. }
            | LibraryAction::Probe { .. },
        ) => bail!("This action is not a playback command"),
    };
    Ok((command, wait))
}

#[cfg(test)]
mod tests {
    use super::*;

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
