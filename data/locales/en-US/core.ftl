core-ready = Rivu core ready. Data: { $path }
core-command-accepted = Command accepted at revision { $revision }
core-stats = Tracks: { $tracks }
    Plays: { $plays }
core-listing-changed = Listing incomplete: data changed while reading; retry the command.
core-missing = [missing]
core-favorite = [favorite]
core-unknown = unknown
core-playing = playing
core-paused = paused
core-stopped = stopped
core-play = Play
core-pause = Pause
core-previous = Previous
core-next = Next
core-shuffle = Shuffle
core-shuffle-on = Shuffle ✓
core-repeat = Repeat
core-repeat-on = Repeat ✓
core-quit = Quit
core-playlist-summary = { $id }	{ $name }	{ $count } entries
core-playlist-entry = { $index }	entry { $entry }	track { $track }
core-relocation-accepted = Relocation verification accepted: task { $id }
core-relocation-completed = Relocation committed: old track { $source } now uses target { $target }'s path
core-duplicate-groups = Historical same-content groups: { $count } (cached hashes, not fresh verification)
core-duplicate-members = Historical same-content members: { $count }
core-duplicate-group = { $fingerprint }	{ $total } members	{ $missing } missing	{ $present } present
core-duplicate-member = { $id }	{ $path }	{ $status }	favorite={ $favorite } overrides={ $overrides } plays={ $plays } playlist_refs={ $playlists } queue_refs={ $queue } independent={ $independent }
core-present = present
core-optimization = Database optimization completed
    Database: { $before } -> { $after } bytes
    WAL: { $wal-before } -> { $wal-after } bytes
    Reclaimed total: { $reclaimed } bytes
core-state-track = { $status }: { $artist } — { $title }  { $position }/{ $duration } s  volume { $volume }%
core-state-empty = { $status } · { $tracks } tracks · { $queued } queued
core-ffmpeg = FFmpeg extension audio decoding: { $status }
core-scan-progress = Scan: { $phase } · { $processed }/{ $total } · { $errors } errors
core-probe = { $path }
      title: { $title }
      artist: { $artist }
      album: { $album }
      codec: { $codec }
      channels: { $channels }
      sample rate: { $sample-rate } Hz
      duration: { $duration }
      bitrate: { $bitrate }
      source bits/sample: { $bits }
      track number: { $track }
      disc number: { $disc }
      release date: { $date }
      probe backend: { $probe }
      decoder backend: { $decoder }
      FFmpeg extension audio decoding: { $ffmpeg }
core-help-about = A quiet, local-first music player
core-help-long = Rivu plays local audio with a shared Rust core. Run without a subcommand for the desktop UI; CLI and TUI control that same instance. Optional FFmpeg audio support is loaded on demand for fallback decoding or enhanced probing.
core-help-usage = Usage:
core-help-gui = Open the desktop UI and optionally import paths
core-help-serve = Run the shared core without a GUI (foreground)
core-help-tui = Control the running instance in a minimal terminal UI
core-help-playback = Control playback, volume, and audio output devices
core-help-library = Import and manage tracks, inspect metadata, and view listening information
core-help-queue = Manage the current playback queue
core-help-playlist = Manage, import, and export playlists
core-help-quit = Shut down the running GUI/headless core
core-help-playback-play = Play a library track ID, or resume the current queue
core-help-playback-volume = Set volume as a percentage, 0..100
core-help-playback-devices = List output devices without starting an instance
core-help-playback-device = Select an output device by exact name, or use the default
core-help-library-scan = Import audio files, CUE sheets or directories without blocking playback
core-help-library-list = List the library, optionally filtered by text
core-help-library-edit = Edit library metadata without rewriting media files
core-help-library-remove = Remove library records, never media files
core-help-library-favorite = Set or clear favorites without modifying media files
core-help-library-clean-missing = Remove missing library records and their references, never media files
core-help-library-status = Show playback and library state.
core-help-library-stats = Show library size and aggregate play counts.
core-help-library-history = Show recent tracks, once per track.
core-help-library-probe = Read metadata and check decoder support without starting an instance.
core-help-library-optimize = Reclaim database space and refresh query statistics; stop playback and scans first.
core-help-library-duplicates = Read-only file groups with matching historical content hashes.
core-help-library-relocate = Preserve a missing track's identity at a verified target.
core-help-library-relocation-status = Read retained relocation verification/completion status.
core-help-library-cancel-relocation = Cancel pending relocation verification; never undo a commit.
core-help-library-keep-independent = Exclude this file from future identity reuse.
core-help-playlist-import = Import an M3U/M3U8 playlist or CUE sheet in track order.
core-help-library-scan-paths = Paths relative to this client's working directory, or absolute paths.
core-help-library-scan-force = Reprobe metadata even when file size and modification time are unchanged.
core-help-library-probe-enhanced = Load FFmpeg for enhanced probing regardless of the saved setting.
core-help-library-duplicates-fingerprint = List members of one hash instead of group summaries.
core-help-library-relocate-wait = Wait for fresh verification and the atomic database commit.
core-help-playlist-import-path = Playlist path relative to this client's working directory, or absolute.
core-help-playlist-export-path = Destination relative to this client's working directory; need not exist.
core-help-commands = Commands
core-help-options = Options
core-help-arguments = Arguments
core-help-help = Print help
core-help-version = Print version
core-help-help-subcommand = Command to show help for
core-relocation-status = Relocation { $id }: { $source } -> { $target } · { $phase }
core-verifying = verifying
core-completed = completed
core-cancelled = cancelled
core-failed = failed
core-discovering = discovering files
core-reading-metadata = reading metadata
core-saving = saving
core-yes = yes
core-no = no
core-missing-marker = { " " }[missing]
core-favorite-marker = { " " }[favorite]
core-last-scan-missing = missing (last scan)
core-last-scan-present = present (last scan)
core-help-data-dir = Directory for Rivu data.
core-help-config = Configuration file to load.
core-help-json = Print machine-readable JSON output.
