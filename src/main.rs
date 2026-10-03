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
    long_about = "Rivu plays local audio with a shared Rust core. Run without a subcommand for the desktop UI; CLI and TUI control that same instance. No FFmpeg or GStreamer backend."
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
    /// Import files or directories without blocking playback.
    Scan {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        wait: bool,
    },
    /// List the library, optionally filtered by text.
    List {
        #[arg(short, long)]
        query: Option<String>,
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
    /// Show the running instance's playback and library state.
    Status,
    /// Show library size and listening totals.
    Stats,
    /// Show recent listening history.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read real metadata and check decoder support without starting an instance.
    Probe { path: PathBuf },
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
    Import {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        wait: bool,
    },
    Export {
        playlist_id: i64,
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
            let info = audio::probe(&path)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": path, "title": info.title, "artist": info.artist, "album": info.album,
                        "codec": info.codec, "channels": info.channels, "sample_rate": info.sample_rate,
                        "duration": info.duration,
                    }))?
                );
                return Ok(());
            }
            println!(
                "{}\n  title: {}\n  artist: {}\n  album: {}\n  codec: {}\n  channels: {}\n  sample rate: {} Hz\n  duration: {}",
                path.display(),
                info.title,
                info.artist,
                info.album,
                info.codec,
                info.channels,
                info.sample_rate,
                info.duration
                    .map_or("unknown".into(), |value| format!("{value:.3} s"))
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
        Action::Library(LibraryAction::List { query }) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            let query = query.unwrap_or_default().to_lowercase();
            for track in response.state.library.iter().filter(|track| {
                query.is_empty()
                    || format!(
                        "{} {} {} {}",
                        track.title,
                        track.artist,
                        track.album,
                        track.path.display()
                    )
                    .to_lowercase()
                    .contains(&query)
            }) {
                println!(
                    "{}\t{}\t{}\t{}\t{}{}",
                    track.id,
                    track.title,
                    track.artist,
                    track.album,
                    track.path.display(),
                    if track.missing { " [missing]" } else { "" }
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
                "Tracks: {}\nPlays: {}\nListened: {:.2} hours",
                response.state.library.len(),
                response
                    .state
                    .library
                    .iter()
                    .map(|t| t.play_count)
                    .sum::<u64>(),
                response
                    .state
                    .library
                    .iter()
                    .map(|t| t.listen_seconds)
                    .sum::<f64>()
                    / 3600.0
            );
            Ok(())
        }
        Action::Library(LibraryAction::History { limit }) => {
            let response = checked(ipc::request(&socket, &Command::Status)?)?;
            if args.json {
                return show(&response, true);
            }
            for entry in response.state.history.iter().take(limit) {
                println!(
                    "{}\t{}\t{:.2}s\t{}\t{}",
                    entry.started_at,
                    entry.title,
                    entry.listened_seconds,
                    if entry.counted { "counted" } else { "partial" },
                    entry.reason
                );
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
                        match mpris::Mpris::start(media_handle.clone(), desktop) {
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
        runtime.handle.send(Command::Scan { paths })?;
    }
    if desktop {
        gui::run(runtime.handle.clone(), data_dir.join("workspace.json"))?;
        if !runtime.handle.snapshot().shutting_down {
            runtime.handle.send(Command::Shutdown)?;
        }
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
        if let Some(error) = &state.last_error {
            eprintln!("{error}");
        }
    }
    if !response.ok {
        bail!("{}", response.error.as_deref().unwrap_or("Command failed"));
    }
    Ok(())
}
fn translate(action: Action) -> Result<(Command, bool)> {
    let mut wait = false;
    let command = match action {
        Action::Library(LibraryAction::Scan {
            paths,
            wait: should_wait,
        }) => {
            wait = should_wait;
            Command::Scan { paths }
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
                Command::ImportPlaylist { path, name }
            }
            PlaylistAction::Export { playlist_id, path } => {
                Command::ExportPlaylist { playlist_id, path }
            }
            PlaylistAction::List => unreachable!(),
        },
        Action::Library(LibraryAction::Remove { track_ids }) => Command::RemoveTracks { track_ids },
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
