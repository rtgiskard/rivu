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
        let mut session = rivu_client::client_session(data_dir);
        session.get_state(rivu_protocol::response::StateSections::default())?;
        let instance = session
            .instance_id()
            .expect("fixture Hello binds Core identity");
        let frontend = FrontendRuntime::connect(data_dir, instance)?;
        let handle = frontend.handle.clone();
        Ok(Self {
            handle,
            frontend: Some(frontend),
            _fixture: fixture,
        })
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

pub(crate) fn artwork_track(path: std::path::PathBuf) -> rivu_protocol::model::Track {
    let fingerprint = path
        .is_file()
        .then(|| std::fs::read(&path).unwrap())
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string());
    rivu_protocol::model::Track {
        id: 1,
        path,
        fingerprint,
        cue: None,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: None,
        codec: "wav".into(),
        channels: 1,
        sample_rate: 8_000,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: Some(16),
        release_date: None,
        favorite: false,
        decoder_hint: Default::default(),
        missing: false,
        play_count: 0,
        last_played: None,
    }
}
