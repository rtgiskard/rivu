use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use rivu::i18n::Localizer;
use rivu::{
    app::App,
    audio, config, ipc,
    model::{Command, MAX_QUERY_ROWS, PlaybackStatus, Query, RepeatMode},
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
    long_about = "Rivu plays local audio with a shared Rust core. Run without a subcommand for the desktop UI; CLI and TUI control that same instance. Optional FFmpeg audio support is loaded on demand for enabled fallback decoding or explicit enhanced probing."
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
        percent: u8,
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
    Probe {
        path: PathBuf,
        /// Explicitly load FFmpeg for enhanced probing, regardless of the saved setting.
        #[arg(long)]
        enhanced: bool,
    },
    /// Reclaim database space and refresh query statistics; stop playback and scans first.
    Optimize,
    /// Read-only ordinary-file groups with matching historical content hashes.
    Duplicates {
        /// List members of one hash instead of group summaries.
        #[arg(long)]
        fingerprint: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Explicitly preserve a missing old track's identity at a verified target.
    Relocate {
        source_id: i64,
        target_id: i64,
        /// Wait for fresh verification and the atomic database commit.
        #[arg(long)]
        wait: bool,
    },
    /// Read retained relocation verification/completion status.
    RelocationStatus { id: u64 },
    /// Cancel a pending relocation verification (never undo a committed one).
    CancelRelocation { id: u64 },
    /// Prevent this ordinary row from participating in future identity reuse.
    KeepIndependent { track_id: i64 },
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
// Resolve the same configuration before clap renders --help. Only path options
// are inspected here; clap remains authoritative for syntax and diagnostics.
fn startup_config_path() -> PathBuf {
    let mut config_path = None;
    let mut data_dir = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            config_path = args.next().map(PathBuf::from);
        } else if arg == "--data-dir" {
            data_dir = args.next().map(PathBuf::from);
        } else if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--config=")) {
            config_path = Some(value.into());
        } else if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--data-dir=")) {
            data_dir = Some(PathBuf::from(value));
        }
    }
    config_path.unwrap_or_else(|| {
        data_dir.map_or_else(config::default_path, |directory| {
            directory.join("config.toml")
        })
    })
}

fn localize_help(command: &mut clap::Command, i18n: &Localizer, path: &str) {
    let messages = i18n
        .fixed_texts()
        .collect::<std::collections::HashMap<_, _>>();
    localize_command_help(command, i18n, path, &messages);
}

fn localize_command_help(
    command: &mut clap::Command,
    i18n: &Localizer,
    path: &str,
    messages: &std::collections::HashMap<&str, &str>,
) {
    let id = if command.get_name() == "help" {
        "core-help-help".to_owned()
    } else if path.is_empty() {
        "core-help-about".to_owned()
    } else {
        format!("core-help-{path}")
    };
    if let Some(text) = messages.get(id.as_str()) {
        *command = command
            .clone()
            .about((*text).to_owned())
            .long_about((*text).to_owned());
    }
    if path.is_empty() {
        *command = command
            .clone()
            .long_about(i18n.text("core-help-long").to_owned());
    }
    *command = command.clone()
        .help_template(format!("{{before-help}}{{name}} {{version}}\n{{about-with-newline}}\n{} {{usage}}\n\n{{all-args}}{{after-help}}", i18n.text("core-help-usage")))
        .subcommand_help_heading(i18n.text("core-help-commands").to_owned());
    let arguments = command
        .get_arguments()
        .map(|arg| (arg.get_id().to_string(), arg.is_positional()))
        .collect::<Vec<_>>();
    for (name, positional) in arguments {
        let name_key = name.replace('_', "-");
        let id = if path.is_empty() {
            format!("core-help-{name_key}")
        } else {
            format!("core-help-{path}-{name_key}")
        };
        let text = if name == "help" {
            Some(i18n.text("core-help-help"))
        } else if name == "version" {
            Some(i18n.text("core-help-version"))
        } else if matches!(name.as_str(), "data_dir" | "config" | "json") {
            messages
                .get(format!("core-help-{name_key}").as_str())
                .copied()
        } else {
            messages.get(id.as_str()).copied()
        };
        *command = command.clone().mut_arg(name.clone(), |arg| {
            let arg = arg.help_heading(
                i18n.text(if positional {
                    "core-help-arguments"
                } else {
                    "core-help-options"
                })
                .to_owned(),
            );
            if let Some(text) = text {
                arg.help(text.to_owned()).long_help(text.to_owned())
            } else {
                arg
            }
        });
    }
    for child in command.get_subcommands_mut() {
        let child_path = if path.is_empty() {
            child.get_name().to_owned()
        } else {
            format!("{path}-{}", child.get_name())
        };
        localize_command_help(child, i18n, &child_path, messages);
    }
}

fn run() -> Result<()> {
    let startup_config = config::Config::load(&startup_config_path());
    // Client-side localization is best-effort; hosts and probes validate their
    // configuration when they initialize, not before remote commands dispatch.
    let language = startup_config
        .as_ref()
        .map_or("auto", |config| config.ui_language.as_str());
    let i18n = Localizer::new(language);
    let mut command = Args::command();
    command.build();
    localize_help(&mut command, &i18n, "");
    let args = Args::from_arg_matches(&command.get_matches())?;
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
                        ("path", path.display().to_string().into()),
                        ("title", info.title.as_str().into()),
                        ("artist", info.artist.as_str().into()),
                        ("album", info.album.as_str().into()),
                        ("codec", info.codec.as_str().into()),
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
                        ("date", info.release_date.as_deref().unwrap_or("—").into()),
                        ("probe", result.probe_backend.to_string().into()),
                        ("decoder", result.decoder_backend.to_string().into()),
                        ("ffmpeg", decoder_status.into()),
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
                    println!("{device}");
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
                println!("{}\t{}\t{}", entry.played_at, entry.track_id, entry.title);
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
                                        ("fingerprint", group.fingerprint.into()),
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
                                        ("path", member.track.path.display().to_string().into()),
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
                    eprintln!("{error}");
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
                                eprintln!("{error}");
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

fn list_queue(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = ipc::client_session(data_dir)?;
    let state = session.get_state(StateSections {
        queue: true,
        ..StateSections::default()
    })?;
    let current = state
        .queue
        .context("State omitted queue section")?
        .current_id;
    let mut after = None;
    let mut expected = None;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut first = true;
    if json {
        write!(output, "[")?;
    }
    loop {
        let response = session.query(&Query::QueueBatch {
            after,
            limit: MAX_QUERY_ROWS,
            expected,
        })?;
        expected.get_or_insert(response.revisions);
        let view = match listing_result(i18n, response.result)? {
            Some(view) => view,
            None => return Ok(()),
        };
        let ViewResponse::QueueBatch(batch) = view else {
            bail!("Queue query returned an unexpected response");
        };
        for row in &batch.rows {
            if json {
                if !first {
                    write!(output, ",")?;
                }
                serde_json::to_writer(&mut output, row)?;
                first = false;
            } else {
                writeln!(
                    output,
                    "{}\t{}\t{}\t{}{}",
                    row.position,
                    row.id,
                    row.track_id,
                    row.track.title,
                    if current == Some(row.id) { " *" } else { "" }
                )?;
            }
        }
        let Some(next) = batch.next else {
            break;
        };
        if batch.rows.is_empty() || after == Some(next) {
            bail!("Queue query returned a non-progressing cursor");
        }
        after = Some(next);
    }
    if json {
        writeln!(output, "]")?;
    }
    Ok(())
}

fn list_library(
    i18n: &Localizer,
    data_dir: &Path,
    json: bool,
    query: Option<String>,
    favorites: bool,
    missing: bool,
) -> Result<()> {
    let mut session = ipc::client_session(data_dir)?;
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
        let view = match listing_result(i18n, response.result)? {
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

fn list_playlists(i18n: &Localizer, data_dir: &Path, json: bool) -> Result<()> {
    let mut session = ipc::client_session(data_dir)?;
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
        let view = match listing_result(i18n, response.result)? {
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
                    "{}",
                    i18n.format(
                        "core-playlist-summary",
                        &[
                            ("id", playlist.id.to_string().into()),
                            ("name", playlist.name.as_str().into()),
                            ("count", playlist.entry_count.to_string().into()),
                        ]
                    )
                )?;
            }
            let mut entry_after = None;
            // Catalog validity spans the listing; entry order is scoped to this
            // playlist and captured by its first batch.
            let mut entry_expected = expected;
            let mut entry_index = 0usize;
            let mut first_entry = true;
            loop {
                let response = session.query(&Query::PlaylistEntryBatch {
                    playlist_id: playlist.id,
                    after: entry_after,
                    limit: MAX_QUERY_ROWS,
                    expected: entry_expected,
                })?;
                entry_expected = Some(response.revisions);
                let view = match listing_result(i18n, response.result)? {
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
                            "  {}",
                            i18n.format(
                                "core-playlist-entry",
                                &[
                                    ("index", entry_index.to_string().into()),
                                    ("entry", entry.id.to_string().into()),
                                    ("track", entry.track_id.to_string().into()),
                                ]
                            )
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

fn listing_result<T>(
    i18n: &Localizer,
    result: std::result::Result<T, QueryError>,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(QueryError::RevisionChanged) => {
            eprintln!("{}", i18n.text("core-listing-changed"));
            Ok(None)
        }
        Err(error) => Err(anyhow::Error::from(error)),
    }
}

fn show_ack(i18n: &Localizer, ack: &rivu::response::Ack, json: bool) -> Result<()> {
    if !ack.ok {
        bail!("{}", ack.error.as_deref().unwrap_or("Command failed"));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(ack)?);
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
fn show_state(i18n: &Localizer, response: &StateResponse, json: bool) -> Result<()> {
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
        PlaybackStatus::Stopped => i18n.text("core-stopped"),
        PlaybackStatus::Playing => i18n.text("core-playing"),
        PlaybackStatus::Paused => i18n.text("core-paused"),
    };
    if let Some(track) = playback.current_track.as_deref() {
        println!(
            "{}",
            i18n.format(
                "core-state-track",
                &[
                    ("status", status.into()),
                    ("artist", track.artist.as_str().into()),
                    ("title", track.title.as_str().into()),
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
        );
    } else {
        let queued = response.queue.as_ref().map_or(0, |queue| queue.total);
        println!(
            "{}",
            i18n.format(
                "core-state-empty",
                &[
                    ("status", status.into()),
                    ("tracks", library.track_total.to_string().into()),
                    ("queued", queued.to_string().into()),
                ]
            )
        );
    }
    if let Some(progress) = &system.scan_progress {
        show_scan(i18n, progress);
    }
    println!(
        "{}",
        i18n.format(
            "core-ffmpeg",
            &[("status", system.ffmpeg_status.as_str().into())]
        )
    );
    if let Some(error) = &system.last_error {
        eprintln!("{error}");
    }
    Ok(())
}
fn show_scan(i18n: &Localizer, progress: &rivu::model::ScanProgress) {
    let phase = i18n.text(match progress.phase {
        rivu::model::ScanPhase::Discovering => "core-discovering",
        rivu::model::ScanPhase::ReadingMetadata => "core-reading-metadata",
        rivu::model::ScanPhase::Saving => "core-saving",
        rivu::model::ScanPhase::Completed => "core-completed",
        rivu::model::ScanPhase::Failed => "core-failed",
        rivu::model::ScanPhase::Cancelled => "core-cancelled",
    });
    println!(
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
    );
    if let Some(path) = &progress.path {
        println!("{}", path.display());
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
