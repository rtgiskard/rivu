//! Filesystem scanning and playlist I/O, independent of database ownership.

mod cue;
mod input;
mod paths;
mod playlist_io;
mod scan;

pub(crate) use paths::logical_path;
pub(crate) use playlist_io::prepare_playlist_import;
pub use playlist_io::{M3uItem, export_m3u};
#[cfg(test)]
pub use playlist_io::{import_m3u, import_playlist};
pub use scan::{KnownFile, ScanCancellation, ScanResult, ScanUncertainty};
pub(crate) use scan::{SOURCE_BATCH_SIZE, scan_paths_with_cues};
#[cfg(test)]
pub use scan::{ScanRecord, SkippedDirectory, scan_paths};

#[cfg(test)]
mod tests;
