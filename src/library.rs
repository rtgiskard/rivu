//! Filesystem scanning and playlist I/O, independent of database ownership.

mod admission;
mod cue;
mod input;
mod paths;
mod playlist_io;
mod scan;

pub(crate) use admission::{ExternalAdmission, prepare_external_admission};
pub(crate) use paths::{logical_path, normalize_roots};
pub(crate) use playlist_io::prepare_playlist_import;
pub use playlist_io::{M3uItem, export_m3u};
#[cfg(test)]
pub use playlist_io::{import_m3u, import_playlist};
pub use scan::{KnownFile, ScanCancellation, ScanResult, ScanUncertainty};
pub(crate) use scan::{SOURCE_BATCH_SIZE, recheck_sources, scan_paths_with_cues};
#[cfg(test)]
pub use scan::{ScanRecord, SkippedDirectory, scan_paths};

#[cfg(test)]
mod tests;
