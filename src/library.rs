//! Filesystem scanning and playlist I/O, independent of database ownership.

mod admission;
mod cue;
mod input;
mod paths;
mod playlist_io;
mod scan;
mod source;

pub(crate) use admission::{ExternalAdmission, prepare_external_admission};
pub(crate) use paths::{logical_path, normalize_roots};
pub(crate) use playlist_io::prepare_playlist_import;
pub use playlist_io::{M3uItem, export_m3u};
#[cfg(test)]
pub use playlist_io::{import_m3u, import_playlist};
pub use scan::{ScanCancellation, ScanResult, ScanUncertainty};
#[cfg(test)]
pub use scan::{SkippedDirectory, scan_paths};
pub(crate) use scan::{recheck_sources, scan_paths_with_cues};
pub use source::KnownFile;
pub(crate) use source::SOURCE_BATCH_SIZE;
#[cfg(test)]
pub use source::ScanRecord;

#[cfg(test)]
mod tests;
