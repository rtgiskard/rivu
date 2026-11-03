//! In-process graphical frontend; the application injects its business host.
mod gui;
pub mod host;
mod projection;
mod runtime;
#[cfg(test)]
mod test_support;

pub use gui::session::LoadedLayouts;
pub use host::{BuildInfo, Host, HostHandle};
pub use runtime::GuiHost;

pub(crate) fn artwork_source(
    track: &rivu_protocol::model::Track,
) -> rivu_metadata::artwork::Source<'_> {
    rivu_metadata::artwork::Source {
        id: track.id,
        fingerprint: track.fingerprint.as_deref(),
        path: &track.path,
    }
}
