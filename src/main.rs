use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use crossbeam_channel::{bounded, select};
use rivu::{
    audio, config,
    core::Runtime,
    gui, ipc,
    model::{Command, MAX_QUERY_ROWS, PlaybackStatus, Query, RepeatMode},
    mpris,
    response::{QueryError, StateResponse, StateSections, ViewResponse},
    terminal,
};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "rivu",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RIVU_GIT_VERSION"), ")"),
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
    /// Show recent tracks, once per track.
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
        Action::Gui { paths } => open_gui(data_dir, config_path, paths),
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
            let response = ipc::get_state(&socket, StateSections::ALL)?;
            show_state(&response, args.json)
        }
        Action::Library(LibraryAction::List {
            query,
            favorites,
            missing,
        }) => list_library(&socket, args.json, query, favorites, missing),
        Action::Library(LibraryAction::Stats) => {
            let response = ipc::query(&socket, &Query::LibraryStats)?;
            let ViewResponse::LibraryStats(stats) = response.result.map_err(anyhow::Error::from)?
            else {
                bail!("Statistics query returned an unexpected response");
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("Tracks: {}\nPlays: {}", stats.total, stats.play_count);
            }
            Ok(())
        }
        Action::Library(LibraryAction::History { limit }) => {
            let response = ipc::get_state(
                &socket,
                StateSections {
                    library: true,
                    ..StateSections::default()
                },
            )?;
            if args.json {
                return show_state(&response, true);
            }
            let state = response
                .library
                .as_ref()
                .context("State omitted library section")?;
            for entry in state.history.iter().take(limit) {
                println!("{}\t{}\t{}", entry.played_at, entry.track_id, entry.title);
            }
            Ok(())
        }
        Action::Queue(QueueAction::List) => {
            let response = ipc::get_state(
                &socket,
                StateSections {
                    queue: true,
                    ..StateSections::default()
                },
            )?;
            if args.json {
                return show_state(&response, true);
            }
            let queue = response
                .queue
                .as_ref()
                .context("State omitted queue section")?;
            for (index, entry) in queue.entries.iter().enumerate() {
                let title = queue
                    .tracks
                    .iter()
                    .find(|t| t.id == entry.track_id)
                    .map_or("Missing track", |t| t.title.as_str());
                println!(
                    "{}\t{}\t{}\t{}{}",
                    index,
                    entry.id,
                    entry.track_id,
                    title,
                    if queue.current_id == Some(entry.id) {
                        " *"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        Action::Playlist(PlaylistAction::List) => list_playlists(&socket, args.json),
        Action::Library(LibraryAction::Edit {
            track_id,
            title,
            artist,
            album,
        }) => {
            let response = ipc::query(&socket, &Query::Track { track_id })?;
            let ViewResponse::Track(track) = response.result.map_err(anyhow::Error::from)? else {
                bail!("Track query returned an unexpected response");
            };
            let track = track.context("Track not found")?;
            let command = Command::EditTrack {
                track_id,
                title: title.unwrap_or(track.title),
                artist: artist.unwrap_or(track.artist),
                album: album.unwrap_or(track.album),
            };
            let ack = ipc::request_ack(&socket, &command)?;
            show_ack(&ack, args.json)
        }
        Action::Library(LibraryAction::Optimize) => {
            let ack = ipc::request_ack(&socket, &Command::OptimizeDatabase)?;
            if !ack.ok {
                return show_ack(&ack, args.json);
            }
            let response = ipc::get_state(
                &socket,
                StateSections {
                    system: true,
                    ..StateSections::default()
                },
            )?;
            if args.json {
                return show_state(&response, true);
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
            let ack = ipc::request_ack(&socket, &command)?;
            if !ack.ok {
                bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
            }
            if wait_scan {
                loop {
                    thread::sleep(Duration::from_millis(200));
                    let response = ipc::get_state(
                        &socket,
                        StateSections {
                            system: true,
                            ..StateSections::default()
                        },
                    )?;
                    let system = response
                        .system
                        .as_ref()
                        .context("State omitted system section")?;
                    if !system.scanning {
                        if let Some(error) = &system.last_error {
                            bail!("{error}");
                        }
                        if args.json {
                            return show_state(&response, true);
                        }
                        println!("{}", system.scan_message);
                        return Ok(());
                    }
                }
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&ack)?);
            } else {
                println!("Command accepted at revision {}", ack.revision);
            }
            Ok(())
        }
    }
}

fn open_gui(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
    let socket = data_dir.join("rivu.sock");
    match ipc::request_ack(&socket, &Command::ShowWindow) {
        Ok(ack) if ack.ok => {
            if !paths.is_empty() {
                let scan = Command::Scan {
                    paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
                    force: false,
                };
                let ack = ipc::request_ack(&socket, &scan)?;
                if !ack.ok {
                    bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
                }
            }
            Ok(())
        }
        Ok(ack) => bail!("{}", ack.error.as_deref().unwrap_or("Command failed")),
        Err(error) if ipc::is_no_instance(&error) => start(data_dir, config_path, paths, true),
        Err(error) => Err(error),
    }
}

fn start(
    data_dir: PathBuf,
    config_path: PathBuf,
    paths: Vec<PathBuf>,
    desktop: bool,
) -> Result<()> {
    let socket = data_dir.join("rivu.sock");
    ipc::bind(&socket)?;
    let mut runtime = match Runtime::start(&data_dir, &config_path) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = std::fs::remove_file(&socket);
            return Err(error);
        }
    };
    let server = ipc::Server::start(socket, runtime.handle.clone())?;
    let media_handle = runtime.handle.clone();
    let media_changes = media_handle.subscribe();
    let (media_stop, media_stop_receiver) = bounded(1);
    let media_stop_for_cleanup = media_stop.clone();
    let media_data_dir = data_dir.clone();
    let media_worker = thread::Builder::new()
        .name("rivu-mpris-manager".into())
        .spawn(move || {
            let mut service = None;
            let mut enabled = false;
            loop {
                let snapshot = media_handle.gui_snapshot();
                if snapshot.system.shutting_down {
                    break;
                }
                if snapshot.system.config.mpris_enabled != enabled {
                    enabled = snapshot.system.config.mpris_enabled;
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
                select! {
                    recv(media_stop_receiver) -> _ => break,
                    recv(media_changes) -> changed => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
            drop(service);
        })?;
    if !paths.is_empty() {
        runtime.handle.send(Command::Scan {
            paths: paths.into_iter().map(client_path).collect::<Result<_>>()?,
            force: false,
        })?;
    }
    let interface_result = if desktop {
        gui::run(runtime.handle.clone(), data_dir.join("workspace.json"))
    } else {
        println!("Rivu core ready. Data: {}", data_dir.display());
        Ok(())
    };
    if interface_result.is_err() {
        let _ = runtime.handle.send(Command::Shutdown);
    }
    let join_result = runtime.join();
    let _ = media_stop_for_cleanup.send(());
    let _ = media_worker.join();
    interface_result?;
    join_result.map_err(|_| anyhow!("Rivu core worker panicked"))?;
    drop(server);
    Ok(())
}
fn list_library(
    socket: &Path,
    json: bool,
    query: Option<String>,
    favorites: bool,
    missing: bool,
) -> Result<()> {
    let mut session = ipc::watch_session(socket)?;
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
        let response = session.query(&Query::TrackBatch {
            query: query.clone(),
            favorite: favorites.then_some(true),
            missing: missing.then_some(true),
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = match listing_result(response.result)? {
            Some(view) => view,
            None => return Ok(()),
        };
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
                    track.title,
                    track.artist,
                    track.album,
                    track.path.display(),
                    if track.missing { " [missing]" } else { "" },
                    if track.favorite { " [favorite]" } else { "" }
                )?;
                match track.bitrate_bps {
                    Some(value) => writeln!(output, "{:.1} kbps", value as f64 / 1000.0)?,
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

fn list_playlists(socket: &Path, json: bool) -> Result<()> {
    let mut session = ipc::watch_session(socket)?;
    let _ = session.get_state(StateSections::default())?;
    let mut expected = None;
    let mut after = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        write!(output, "[")?;
    }
    let mut first_playlist = true;
    loop {
        let response = session.query(&Query::PlaylistSummaryBatch {
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = match listing_result(response.result)? {
            Some(view) => view,
            None => return Ok(()),
        };
        let ViewResponse::PlaylistSummaryBatch(batch) = view else {
            bail!("Playlist query returned an unexpected response");
        };
        for playlist in &batch.rows {
            if json {
                if !first_playlist {
                    write!(output, ",")?;
                }
                write!(output, "{{\"id\":{},\"name\":", playlist.id)?;
                serde_json::to_writer(&mut output, &playlist.name)?;
                write!(
                    output,
                    ",\"entry_count\":{},\"entries\":[",
                    playlist.entry_count
                )?;
            } else {
                writeln!(
                    output,
                    "{}\t{}\t{} entries",
                    playlist.id, playlist.name, playlist.entry_count
                )?;
            }
            let mut entry_after = None;
            let mut entry_index = 0usize;
            let mut first_entry = true;
            loop {
                let response = session.query(&Query::PlaylistEntryBatch {
                    playlist_id: playlist.id,
                    after: entry_after,
                    limit: MAX_QUERY_ROWS,
                    expected,
                })?;
                let view = match listing_result(response.result)? {
                    Some(view) => view,
                    None => return Ok(()),
                };
                let ViewResponse::PlaylistEntryBatch(entries) = view else {
                    bail!("Playlist entries query returned an unexpected response");
                };
                for entry in &entries.rows {
                    if json {
                        if !first_entry {
                            write!(output, ",")?;
                        }
                        serde_json::to_writer(&mut output, entry)?;
                        first_entry = false;
                    } else {
                        writeln!(
                            output,
                            "  {}\tentry {}\ttrack {}",
                            entry_index, entry.id, entry.track_id
                        )?;
                    }
                    entry_index += 1;
                }
                let Some(next) = entries.next else {
                    break;
                };
                entry_after = Some(next);
            }
            if json {
                write!(output, "]}}")?;
                first_playlist = false;
            }
        }
        let Some(next) = batch.next else {
            break;
        };
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

fn listing_result<T>(result: std::result::Result<T, QueryError>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(QueryError::RevisionChanged) => {
            eprintln!("Listing incomplete: data changed while reading; retry the command.");
            Ok(None)
        }
        Err(error) => Err(anyhow::Error::from(error)),
    }
}

fn show_ack(ack: &rivu::response::Ack, json: bool) -> Result<()> {
    if !ack.ok {
        bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(ack)?);
    } else {
        println!("Command accepted at revision {}", ack.revision);
    }
    Ok(())
}
fn show_state(response: &StateResponse, json: bool) -> Result<()> {
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
        PlaybackStatus::Stopped => "stopped",
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
    };
    if let Some(track) = playback.current_track.as_deref() {
        println!(
            "{status}: {} — {}  {:.1}/{} s  volume {:.0}%",
            track.artist,
            track.title,
            playback.playback.position,
            playback
                .playback
                .duration
                .map_or("?".into(), |v| format!("{v:.1}")),
            playback.playback.volume * 100.0
        );
    } else {
        let queued = response
            .queue
            .as_ref()
            .map_or(0, |queue| queue.entries.len());
        println!(
            "{status} · {} tracks · {} queued",
            library.track_total, queued
        );
    }
    if !system.scan_message.is_empty() {
        println!("{}", system.scan_message);
    }
    println!("FFmpeg extension audio decoding: {}", system.ffmpeg_status);
    if let Some(error) = &system.last_error {
        eprintln!("{error}");
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
