pub mod app;
pub mod config;
pub(crate) mod core;
pub(crate) mod execution;
pub(crate) mod gui;
pub mod i18n;
pub mod ipc;
pub(crate) mod keyboard;
pub(crate) mod library;
pub(crate) mod mpris;
mod processing;
pub mod projection;
pub(crate) mod query_tracking;
pub(crate) mod store;
pub mod terminal;
pub(crate) mod theme;
pub(crate) mod tray;

pub(crate) static DEFAULT_IMAGE: &[u8] = include_bytes!("../data/icons/rivu.png");

pub(crate) fn artwork_source(
    track: &rivu_protocol::model::Track,
) -> rivu_metadata::artwork::Source<'_> {
    rivu_metadata::artwork::Source {
        id: track.id,
        fingerprint: track.fingerprint.as_deref(),
        path: &track.path,
    }
}

#[cfg(test)]
mod test_support;
