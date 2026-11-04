//! Graphical remote client and desktop-local presentation lifetime.
mod desktop;
mod frontend;
mod gui;
mod projection;
#[cfg(test)]
mod test_support;
mod transport;

pub use desktop::raise_async;

/// Executable build metadata displayed by the standalone desktop.
#[derive(Clone, Copy, Debug)]
pub struct BuildInfo {
    pub timestamp: i64,
    pub git_version: &'static str,
    pub features: &'static str,
}
impl gpui::Global for BuildInfo {}

/// Attach a desktop to a ready, explicitly identified Core service.
/// The executable owns Core startup and initial path submission.
pub fn run(
    data_dir: std::path::PathBuf,
    expected_instance: u32,
    build_info: BuildInfo,
) -> anyhow::Result<()> {
    desktop::Desktop::open(data_dir, expected_instance, build_info)
}

pub(crate) fn artwork_source(
    track: &rivu_protocol::model::Track,
) -> rivu_metadata::artwork::Source<'_> {
    rivu_metadata::artwork::Source {
        id: track.id,
        fingerprint: track.fingerprint.as_deref(),
        path: &track.path,
    }
}
