//! General-purpose primitives without application state or format dependencies.
//!
//! Filesystem helpers are always available. Process logging and terminal
//! diagnostics require the `logging` and `terminal` features respectively.

#[cfg(feature = "logging")]
pub mod logging;
pub mod regular_file;
#[cfg(any(feature = "terminal", feature = "logging"))]
pub mod terminal;
