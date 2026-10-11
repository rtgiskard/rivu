//! Filesystem scanning and playlist I/O, independent of database ownership.

mod paths;
mod playlist_io;
mod scan;

pub(crate) use paths::logical_path;
pub use playlist_io::{M3uItem, export_m3u, import_m3u, import_playlist};
pub use scan::{KnownFile, ScanRecord, ScanResult, SkippedDirectory, scan_paths};

#[cfg(test)]
mod tests;
