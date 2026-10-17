//! Filesystem scanning and playlist I/O, independent of database ownership.

mod input;
mod paths;
mod playlist_io;
mod scan;

pub(crate) use paths::logical_path;
pub(crate) use playlist_io::prepare_playlist_import;
pub use playlist_io::{M3uItem, export_m3u, import_m3u, import_playlist};
pub(crate) use scan::scan_paths_with_cues;
pub use scan::{
    KnownFile, ScanCancellation, ScanRecord, ScanResult, ScanUncertainty, SkippedDirectory,
    scan_paths,
};

#[cfg(test)]
mod tests;
