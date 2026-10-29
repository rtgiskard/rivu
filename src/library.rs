//! Filesystem scanning and playlist I/O, independent of database ownership.

mod admission;
mod cue;
mod import_budget;
mod input;
mod paths;
mod playlist_io;
mod scan;
mod source;

pub(crate) use admission::{ExternalAdmission, prepare_external_admission};
pub(crate) use import_budget::{ImportBudget, ImportBudgetScope};
pub(crate) use paths::{logical_path, normalize_roots};
pub use playlist_io::M3uItem;
pub(crate) use playlist_io::export_m3u;
pub(crate) use playlist_io::prepare_playlist_import;
#[cfg(test)]
pub use playlist_io::{import_m3u, import_playlist};
pub use scan::{ScanCancellation, ScanResult, ScanUncertainty};
pub(crate) use scan::{ScanSettings, recheck_sources, scan_paths_with_cues};
#[cfg(test)]
pub(crate) use scan::{SkippedDirectory, scan_paths};
pub use source::KnownFile;
pub(crate) use source::SOURCE_BATCH_SIZE;
#[cfg(test)]
pub use source::ScanRecord;
pub(crate) use source::{CueRecord, CueView, RegisteredCueMetadata, ScanTrack};

#[cfg(test)]
mod tests;
