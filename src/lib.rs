pub mod app;
pub mod config;
pub(crate) mod core;
pub(crate) mod execution;
pub mod ipc;
pub(crate) mod library;
pub(crate) mod mpris;
mod processing;
pub(crate) mod projection;
pub(crate) mod store;
#[cfg(feature = "gui")]
pub(crate) mod tray;

pub(crate) fn artwork_source(
    track: &rivu_protocol::model::Track,
) -> rivu_metadata::artwork::Source<'_> {
    rivu_metadata::artwork::Source {
        id: track.id,
        fingerprint: track.fingerprint.as_deref(),
        path: &track.path,
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
