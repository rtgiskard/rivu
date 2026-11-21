use crate::frontend::{FrontendHandle, FrontendRuntime};
use anyhow::Result;
use rivu_format::config::Config;
use std::path::Path;
use std::sync::Arc;

pub(crate) struct CoreRuntime {
    pub(crate) handle: FrontendHandle,
    // Retire remote workers before the fixture's service endpoints disappear.
    frontend: Option<FrontendRuntime>,
    _fixture: rivu::test_support::frontend::FrontendFixture,
}

impl CoreRuntime {
    pub(crate) fn start(data_dir: &Path, config: &Path) -> Result<Self> {
        let fixture = rivu::test_support::frontend::FrontendFixture::start(data_dir, config)?;
        let frontend = FrontendRuntime::connect(data_dir, fixture.instance_id())?;
        let handle = frontend.handle.clone();
        Ok(Self {
            handle,
            frontend: Some(frontend),
            _fixture: fixture,
        })
    }

    pub(crate) fn start_at(directory: &Path) -> Self {
        Self::start(directory, &directory.join("config.toml")).unwrap()
    }

    /// Starts Core with [`save_quiet_config`]; seed storage first by calling
    /// the two separately.
    pub(crate) fn start_quiet(data_dir: &Path) -> Self {
        let config = save_quiet_config(data_dir);
        Self::start(data_dir, &config).unwrap()
    }

    pub(crate) fn disconnect_frontend(&mut self) {
        self.frontend.take();
    }

    pub(crate) fn executor(&self) -> &Arc<async_executor::Executor<'static>> {
        self._fixture.executor()
    }
}

pub(crate) fn build_info() -> crate::BuildInfo {
    crate::BuildInfo {
        timestamp: 0,
        git_version: "test",
        features: "test-support",
    }
}

pub(crate) trait ConfigFixture {
    fn save_fixture(&self, path: &Path) -> Result<()>;
    fn load_fixture(path: &Path) -> Result<Config>;
}
impl ConfigFixture for Config {
    fn save_fixture(&self, path: &Path) -> Result<()> {
        rivu::test_support::frontend::save_config(self, path)
    }
    fn load_fixture(path: &Path) -> Result<Config> {
        rivu::test_support::frontend::load_config(path)
    }
}

/// Writes `config.toml` for a muted Core without FFmpeg or MPRIS.
pub(crate) fn save_quiet_config(directory: &Path) -> std::path::PathBuf {
    let path = directory.join("config.toml");
    Config {
        volume: 0.0,
        ffmpeg_enabled: false,
        mpris_enabled: false,
        ..Default::default()
    }
    .save_fixture(&path)
    .unwrap();
    path
}

/// A catalog row whose fields are irrelevant except its identity; tests
/// override the fields they assert with struct update syntax.
pub(crate) fn library_row(id: i64) -> rivu_protocol::model::LibraryRow {
    rivu_protocol::model::LibraryRow {
        id,
        path: Default::default(),
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: None,
        codec: String::new(),
        channels: 0,
        sample_rate: 0,
        bits_per_sample: None,
        cue_number: None,
        bitrate_bps: None,
        bitrate_exact: false,
        favorite: false,
        missing: false,
        play_count: 0,
    }
}

pub(crate) fn relocation_artwork_paths(directory: &Path) -> [std::path::PathBuf; 2] {
    [("before", [255, 0, 0]), ("after", [0, 255, 0])].map(|(name, colour)| {
        let path = directory.join(name);
        std::fs::create_dir(&path).unwrap();
        image::RgbImage::from_pixel(2, 2, image::Rgb(colour))
            .save(path.join("cover.png"))
            .unwrap();
        path.join("track.wav")
    })
}
