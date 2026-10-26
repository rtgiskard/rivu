use super::output::Terminal;
use clap::{Parser, Subcommand, ValueEnum};
use rivu::i18n::Localizer;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rivu",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RIVU_GIT_VERSION"), ")"),
    about = "A quiet, local-first music player",
    long_about = "Run without a command to open the desktop UI. GUI and serve own one shared core per data directory; control commands connect to it. Data/config options select startup storage and configuration, not live settings."
)]
pub(super) struct Args {
    #[arg(long, global = true, value_name = "DIRECTORY")]
    pub(super) data_dir: Option<PathBuf>,
    #[arg(long, global = true, value_name = "FILE")]
    pub(super) config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub(super) json: bool,
    #[command(subcommand)]
    pub(super) action: Option<Action>,
}

#[derive(Subcommand)]
pub(super) enum Action {
    /// Open the desktop UI and optionally import paths.
    Gui { paths: Vec<PathBuf> },
    /// Run the shared core without a GUI (foreground).
    Serve { paths: Vec<PathBuf> },
    /// Control the running instance in a minimal terminal UI.
    Tui,
    /// Show playback and library state from the running instance.
    Status,
    /// Inspect or cancel retained background tasks.
    #[command(subcommand)]
    Task(TaskAction),
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
    /// Configure audio processing, run offline jobs, and audition retained results.
    #[command(subcommand)]
    Processing(ProcessingAction),
    /// Shut down the running GUI/headless core.
    Quit,
}

#[derive(Subcommand)]
pub(super) enum PlaybackAction {
    /// Play a library track ID, or resume the current queue.
    Play {
        track_id: Option<i64>,
    },
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        #[arg(value_parser = parse_nonnegative_seconds)]
        seconds: f64,
    },
    /// Set volume as a percentage, 0..100.
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
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
pub(super) enum ProcessingAction {
    /// Show authoritative equalizer, denoise, neural settings and latest task.
    Show,
    /// Bypass all playback effects, keep their parameters and offline tasks.
    Bypass,
    /// Inspect, edit and execute the persisted audio node graph.
    #[command(subcommand)]
    Graph(ProcessingGraphAction),
    /// Apply a field-based EqualizerEdit JSON value (including presets and A/B).
    Eq { edit: String },
    /// Merge a DenoisePatch JSON object into current settings.
    Denoise { patch: String },
    /// Merge local interpreter/manifest settings from a NeuralPatch JSON object.
    Neural { patch: String },
    /// Process one library source without modifying it. Separation requires stereo.
    Start {
        track_id: i64,
        #[arg(value_enum)]
        operation: ProcessingKind,
        #[arg(long)]
        wait: bool,
        #[arg(long, requires = "noise_end", value_parser = parse_nonnegative_seconds)]
        noise_start: Option<f64>,
        #[arg(long, requires = "noise_start", value_parser = parse_nonnegative_seconds)]
        noise_end: Option<f64>,
    },
    /// Audition retained outputs, starting their original source queue item if needed.
    Audition {
        #[arg(value_name = "TASK_ID")]
        id: u64,
        #[arg(value_enum)]
        mode: ProcessingAudition,
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true, value_parser = parse_gain)]
        vocal_gain_db: f32,
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true, value_parser = parse_gain)]
        accompaniment_gain_db: f32,
    },
}

#[derive(Subcommand)]
pub(super) enum ProcessingGraphAction {
    /// Show the authoritative graph, including its optimistic edit revision.
    Show,
    /// Apply one GraphEdit JSON value against the required current revision.
    Edit {
        #[arg(allow_hyphen_values = true)]
        edit: String,
        #[arg(long = "expected-revision")]
        revision: u64,
    },
    /// Process a track through the selected node output and its ancestors only.
    Run {
        track_id: i64,
        #[arg(value_name = "NODE_ID")]
        node: u64,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, value_enum, default_value = "auto")]
        mode: ProcessingGraphMode,
        #[arg(long)]
        wait: bool,
    },
    /// Audition a node output retained by a completed graph run.
    Audition {
        #[arg(value_name = "TASK_ID")]
        id: u64,
        #[arg(value_name = "NODE_ID")]
        node: u64,
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Render a retained graph snapshot offline to a path in this client's directory.
    Export {
        #[arg(value_name = "TASK_ID")]
        id: u64,
        #[arg(value_name = "NODE_ID")]
        node: u64,
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ProcessingGraphMode {
    Auto,
    Offline,
    Realtime,
    Hybrid,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ProcessingKind {
    Denoise,
    Speech,
    Separate,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ProcessingAudition {
    Original,
    Denoised,
    Vocals,
    Accompaniment,
    Mix,
}

#[derive(Subcommand)]
pub(super) enum TaskAction {
    /// Read the retained state of a task, including terminal failures.
    Status {
        #[arg(value_enum)]
        kind: TaskKind,
        #[arg(value_name = "TASK_ID")]
        id: u64,
    },
    /// Request cancellation without undoing committed work.
    Cancel {
        #[arg(value_enum)]
        kind: TaskKind,
        #[arg(value_name = "TASK_ID")]
        id: u64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum TaskKind {
    Scan,
    Relocation,
    Processing,
}

#[derive(Subcommand)]
pub(super) enum LibraryAction {
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
    /// Browse and check registered tracks not covered by current configured roots; with no roots, includes all registered tracks.
    #[command(subcommand)]
    Other(OtherTracksAction),
    /// List the library, optionally filtered by text.
    List {
        #[arg(short, long)]
        query: Option<String>,
        #[arg(long, action = clap::ArgAction::Set)]
        favorite: Option<bool>,
        #[arg(long, action = clap::ArgAction::Set)]
        missing: Option<bool>,
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
        #[arg(value_enum)]
        set: Switch,
        #[arg(required = true)]
        track_ids: Vec<i64>,
    },
    /// Remove missing library records and their references, never media files.
    CleanMissing,
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
        #[arg(long, default_value_t = 100, value_parser = parse_page_limit)]
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
    /// Prevent this ordinary row from participating in future identity reuse.
    KeepIndependent { track_id: i64 },
}

#[derive(clap::Args)]
pub(super) struct OtherTracksFilter {
    #[arg(short, long)]
    pub(super) query: Option<String>,
    /// Match favorite or non-favorite tracks; omit to include both.
    #[arg(long, action = clap::ArgAction::Set)]
    pub(super) favorite: Option<bool>,
    /// Match missing or present tracks; omit to include both.
    #[arg(long, action = clap::ArgAction::Set)]
    pub(super) missing: Option<bool>,
    #[arg(long, value_enum, default_value = "id")]
    pub(super) sort: LibraryOrder,
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum LibraryOrder {
    Id,
    Album,
    MostPlayed,
}

impl From<LibraryOrder> for rivu_protocol::model::LibrarySort {
    fn from(value: LibraryOrder) -> Self {
        match value {
            LibraryOrder::Id => Self::Id,
            LibraryOrder::Album => Self::Album,
            LibraryOrder::MostPlayed => Self::MostPlayed,
        }
    }
}

#[derive(Subcommand)]
pub(super) enum OtherTracksAction {
    /// Read one bounded page of tracks not covered by current configured roots; with no roots, includes all registered tracks.
    List {
        #[command(flatten)]
        filter: OtherTracksFilter,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100, value_parser = parse_page_limit)]
        limit: usize,
    },
    /// Count tracks not covered by current configured roots; with no roots, counts all registered tracks.
    Count,
    /// Locate a track outside current configured roots in the filtered and sorted Other view; with no roots, searches all registered tracks.
    Locate {
        track_id: i64,
        #[command(flatten)]
        filter: OtherTracksFilter,
    },
    /// Check registered files not covered by current configured roots; with no roots, checks all registered tracks.
    Check {
        #[arg(long)]
        wait: bool,
        /// Reread metadata even when file size and modification time are unchanged.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Switch {
    On,
    Off,
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Repeat {
    Off,
    All,
    One,
}
#[derive(Subcommand)]
pub(super) enum QueueAction {
    List,
    Add {
        #[arg(required = true)]
        track_ids: Vec<i64>,
    },
    /// Append registered tracks not covered by current configured roots; with no roots, appends all registered tracks.
    AddOther,
    /// Admit ordinary local audio files to the queue without configuring roots.
    AddPaths {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Insert before this queue index instead of appending.
        #[arg(long, requires = "expected_revision")]
        index: Option<usize>,
        /// Queue revision observed when choosing the insertion index.
        #[arg(long, requires = "index")]
        expected_revision: Option<u64>,
        /// Wait for admission to finish using the shared media task lifecycle.
        #[arg(long)]
        wait: bool,
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
pub(super) enum PlaylistAction {
    List,
    Show {
        playlist_id: i64,
    },
    Create {
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

fn parse_nonnegative_seconds(value: &str) -> Result<f64, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| "expected seconds as a number")?;
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(seconds)
    } else {
        Err("seconds must be finite and nonnegative".to_owned())
    }
}

fn parse_gain(value: &str) -> Result<f32, String> {
    let gain = value.parse::<f32>().map_err(|_| "expected gain in dB")?;
    if gain.is_finite() && (-96.0..=24.0).contains(&gain) {
        Ok(gain)
    } else {
        Err("gain must be finite and between -96 and 24 dB".to_owned())
    }
}

fn parse_page_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "expected a page limit from 0 to 256")?;
    if limit <= 256 {
        Ok(limit)
    } else {
        Err("page limit must not exceed 256".to_owned())
    }
}

impl Args {
    /// Validate relationships that require parsed values or command-line provenance.
    pub(super) fn validate(&self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        let invalid = |message| clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message);
        if self.json
            && matches!(
                self.action,
                None | Some(Action::Gui { .. } | Action::Serve { .. } | Action::Tui)
            )
        {
            return Err(invalid(
                "--json requires a finite control command; it cannot be used with gui, serve, tui, or default GUI startup",
            ));
        }
        match &self.action {
            Some(Action::Library(LibraryAction::Edit {
                title,
                artist,
                album,
                ..
            })) if title.is_none() && artist.is_none() && album.is_none() => {
                return Err(invalid(
                    "library edit requires at least one of --title, --artist, or --album",
                ));
            }
            Some(Action::Processing(ProcessingAction::Start {
                operation,
                noise_start,
                noise_end,
                ..
            })) => {
                if noise_start.is_some() && !matches!(operation, ProcessingKind::Denoise) {
                    return Err(invalid(
                        "--noise-start and --noise-end are only valid for denoise",
                    ));
                }
                if let (Some(start), Some(end)) = (noise_start, noise_end)
                    && end <= start
                {
                    return Err(invalid(
                        "noise interval must have --noise-end greater than --noise-start",
                    ));
                }
            }
            Some(Action::Processing(ProcessingAction::Audition { mode, .. }))
                if !matches!(mode, ProcessingAudition::Mix) =>
            {
                if let Some(audition) = matches
                    .subcommand_matches("processing")
                    .and_then(|processing| processing.subcommand_matches("audition"))
                    && ["vocal_gain_db", "accompaniment_gain_db"]
                        .into_iter()
                        .any(|name| {
                            audition.value_source(name)
                                == Some(clap::parser::ValueSource::CommandLine)
                        })
                {
                    return Err(invalid(
                        "--vocal-gain-db and --accompaniment-gain-db are only valid for mix",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub(super) fn terminal_cli_error(mut error: clap::Error) -> clap::Error {
    use clap::error::{ContextKind, ContextValue};

    // Keep clap's trusted usage layout and styling, escaping only argv values.
    for kind in [
        ContextKind::InvalidArg,
        ContextKind::InvalidSubcommand,
        ContextKind::InvalidValue,
        ContextKind::PriorArg,
    ] {
        if let Some(mut value) = error.remove(kind) {
            match &mut value {
                ContextValue::String(text) => *text = Terminal(&*text).to_string(),
                ContextValue::Strings(texts) => {
                    for text in texts {
                        *text = Terminal(&*text).to_string();
                    }
                }
                _ => {}
            }
            error.insert(kind, value);
        }
    }
    error
}

pub(super) fn localize_help(command: &mut clap::Command, i18n: &Localizer, path: &str) {
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
        *command = std::mem::take(command)
            .about((*text).to_owned())
            .long_about((*text).to_owned());
    }
    if path.is_empty() {
        *command = std::mem::take(command).long_about(i18n.text("core-help-long").to_owned());
    }
    let invocation = command
        .get_bin_name()
        .unwrap_or(command.get_name())
        .to_owned();
    let title = if path.is_empty() {
        "{name} {version}"
    } else {
        "{name}"
    };
    *command = std::mem::take(command)
        .display_name(invocation)
        .help_template(format!("{{before-help}}{title}\n{{about-with-newline}}\n{} {{usage}}\n\n{{all-args}}{{after-help}}", i18n.text("core-help-usage")))
        .subcommand_help_heading(i18n.text("core-help-commands").to_owned());
    let arguments = command
        .get_arguments()
        .map(|arg| (arg.get_id().to_string(), arg.is_positional()))
        .collect::<Vec<_>>();
    for (order, (name, positional)) in arguments.into_iter().enumerate() {
        let global = matches!(
            name.as_str(),
            "data_dir" | "config" | "json" | "help" | "version"
        );
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
            messages.get(id.as_str()).copied().or_else(|| {
                messages
                    .get(format!("core-help-arg-{name_key}").as_str())
                    .copied()
            })
        };
        *command = std::mem::take(command).mut_arg(name, |arg| {
            let arg = arg
                .display_order(if global { 10_000 + order } else { order })
                .help_heading(
                    i18n.text(if global {
                        "core-help-global-options"
                    } else if positional {
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
    for (order, child) in command.get_subcommands_mut().enumerate() {
        *child = std::mem::take(child).display_order(order);
        let child_path = if path.is_empty() {
            child.get_name().to_owned()
        } else {
            format!("{path}-{}", child.get_name())
        };
        localize_command_help(child, i18n, &child_path, messages);
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, terminal_cli_error};
    use clap::CommandFactory;

    #[test]
    fn cli_parse_errors_escape_untrusted_arguments_and_preserve_exit_status() {
        for arguments in [
            vec!["rivu", "\x1b[31m音楽\n"],
            vec!["rivu", "--\x1b[31m音楽\t"],
            vec!["rivu", "library", "history", "--limit", "\u{009b}31m\n"],
        ] {
            let error = Args::command().try_get_matches_from(arguments).unwrap_err();
            let status = error.exit_code();
            let error = terminal_cli_error(error);
            assert_eq!(error.exit_code(), status);
            assert_eq!(status, 2);
            let rendered = error.render().to_string();
            assert!(!rendered.chars().any(|character| {
                character.is_control() && character != '\n' && character != '\t'
            }));
            assert!(rendered.contains("\\x1b") || rendered.contains("\\u{009b}"));
            assert!(rendered.contains("\\n") || rendered.contains("\\t"));
            assert!(rendered.contains('\n'));
        }
    }

    fn parse_and_validate(arguments: &[&str]) -> Result<Args, clap::Error> {
        use clap::FromArgMatches;
        let matches = Args::command().try_get_matches_from(arguments)?;
        let args = Args::from_arg_matches(&matches)?;
        args.validate(&matches)?;
        Ok(args)
    }

    #[test]
    fn favorite_position_and_tristate_filters_preserve_explicit_false() {
        use super::{Action, LibraryAction, Switch};
        assert!(
            matches!(parse_and_validate(&["rivu", "library", "favorite", "off", "2", "3"]).unwrap().action, Some(Action::Library(LibraryAction::Favorite { set: Switch::Off, track_ids })) if track_ids == [2, 3])
        );
        assert!(matches!(
            parse_and_validate(&[
                "rivu",
                "library",
                "list",
                "--favorite",
                "false",
                "--missing",
                "true"
            ])
            .unwrap()
            .action,
            Some(Action::Library(LibraryAction::List {
                favorite: Some(false),
                missing: Some(true),
                ..
            }))
        ));
    }

    #[test]
    fn numeric_domains_and_page_limits_are_enforced() {
        for value in ["-1", "NaN", "inf", "-inf"] {
            assert!(parse_and_validate(&["rivu", "playback", "seek", "--", value]).is_err());
            assert!(
                parse_and_validate(&[
                    "rivu",
                    "processing",
                    "start",
                    "1",
                    "denoise",
                    "--noise-start",
                    value,
                    "--noise-end",
                    "10"
                ])
                .is_err()
            );
        }
        for value in ["-96.1", "24.1", "NaN", "inf", "-inf"] {
            for gain in ["--vocal-gain-db", "--accompaniment-gain-db"] {
                assert!(
                    parse_and_validate(&[
                        "rivu",
                        "processing",
                        "audition",
                        "1",
                        "mix",
                        gain,
                        value
                    ])
                    .is_err()
                );
            }
        }
        for value in ["0", "100"] {
            assert!(parse_and_validate(&["rivu", "playback", "volume", value]).is_ok());
        }
        assert!(parse_and_validate(&["rivu", "playback", "volume", "101"]).is_err());
        assert!(parse_and_validate(&["rivu", "playback", "seek", "0"]).is_ok());
        assert!(
            parse_and_validate(&[
                "rivu",
                "processing",
                "audition",
                "1",
                "mix",
                "--vocal-gain-db",
                "-96",
                "--accompaniment-gain-db",
                "24"
            ])
            .is_ok()
        );
        for command in [
            vec!["library", "duplicates"],
            vec!["library", "other", "list"],
        ] {
            for value in ["0", "256"] {
                let argv = ["rivu"]
                    .into_iter()
                    .chain(command.iter().copied())
                    .chain(["--limit", value])
                    .collect::<Vec<_>>();
                assert!(parse_and_validate(&argv).is_ok());
            }
            let argv = ["rivu"]
                .into_iter()
                .chain(command.iter().copied())
                .chain(["--limit", "257"])
                .collect::<Vec<_>>();
            assert!(parse_and_validate(&argv).is_err());
        }
    }

    #[test]
    fn cross_field_validation_rejects_invalid_combinations_but_accepts_empty_edits() {
        for argv in [
            vec!["rivu", "--json"],
            vec!["rivu", "--json", "gui"],
            vec!["rivu", "serve", "--json"],
            vec!["rivu", "tui", "--json"],
            vec!["rivu", "library", "edit", "1"],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "denoise",
                "--noise-start",
                "0",
            ],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "denoise",
                "--noise-end",
                "1",
            ],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "denoise",
                "--noise-start",
                "1",
                "--noise-end",
                "1",
            ],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "denoise",
                "--noise-start",
                "2",
                "--noise-end",
                "1",
            ],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "speech",
                "--noise-start",
                "0",
                "--noise-end",
                "1",
            ],
            vec![
                "rivu",
                "processing",
                "start",
                "1",
                "separate",
                "--noise-start",
                "0",
                "--noise-end",
                "1",
            ],
        ] {
            assert!(parse_and_validate(&argv).is_err(), "accepted {argv:?}");
        }
        for mode in ["original", "denoised", "vocals", "accompaniment"] {
            assert!(parse_and_validate(&["rivu", "processing", "audition", "1", mode]).is_ok());
            for gain in ["--vocal-gain-db", "--accompaniment-gain-db"] {
                assert!(
                    parse_and_validate(&["rivu", "processing", "audition", "1", mode, gain, "0"])
                        .is_err()
                );
            }
        }
        for field in ["--title", "--artist", "--album"] {
            assert!(parse_and_validate(&["rivu", "library", "edit", "1", field, ""]).is_ok());
        }
        assert!(
            parse_and_validate(&[
                "rivu",
                "processing",
                "start",
                "1",
                "denoise",
                "--noise-start",
                "0",
                "--noise-end",
                "0.5"
            ])
            .is_ok()
        );
        assert!(parse_and_validate(&["rivu", "status", "--json"]).is_ok());
    }
}

#[cfg(test)]
mod graph_tests {
    use super::{Action, Args, ProcessingAction, ProcessingGraphAction};
    use clap::Parser;

    #[test]
    fn graph_edit_requires_explicit_optimistic_revision_and_preserves_negative_json() {
        let edit = r#"{"Replace":{"id":3,"kind":{"mix":{"gains_db":[-6.0,-12.5],"muted":[false,false],"solo":[false,false]}}}}"#;
        assert!(Args::try_parse_from(["rivu", "processing", "graph", "edit", edit]).is_err());
        let args = Args::try_parse_from([
            "rivu",
            "processing",
            "graph",
            "edit",
            "--expected-revision",
            "19",
            edit,
        ])
        .unwrap();
        let Some(Action::Processing(ProcessingAction::Graph(ProcessingGraphAction::Edit {
            edit: parsed,
            revision,
        }))) = args.action
        else {
            panic!("expected graph edit");
        };
        assert_eq!(revision, 19);
        assert_eq!(parsed, edit);
    }

    #[test]
    fn graph_arguments_reject_invalid_ports_modes_and_missing_targets() {
        for arguments in [
            vec!["run", "42"],
            vec!["run", "42", "7", "--mode", "automatic"],
            vec!["run", "42", "7", "--port", "65536"],
            vec!["audition", "9", "7", "--port", "-1"],
            vec!["export", "9", "7"],
        ] {
            assert!(
                Args::try_parse_from(["rivu", "processing", "graph"].into_iter().chain(arguments))
                    .is_err()
            );
        }
    }
}
