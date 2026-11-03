use crate::host::*;
use anyhow::Result;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use rivu_protocol::{
    analysis::{AnalysisFrame, DisplayDemand},
    display::DisplayState,
    model::{Command, Config, ConfigChange, ConfigPatch, Query},
    query_tracking::QueryTicket,
    response::{Ack, QueryResponse},
    waveform::WaveformFrame,
};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

pub(crate) struct CoreRuntime {
    pub(crate) handle: HostHandle,
    _fixture: rivu::test_support::frontend::FrontendFixture,
}

impl CoreRuntime {
    pub(crate) fn start(data_dir: &Path, config: &Path) -> Result<Self> {
        let fixture = rivu::test_support::frontend::FrontendFixture::start(data_dir, config)?;
        let handle = HostHandle::new(TestHost(fixture.endpoint.clone()));
        Ok(Self {
            handle,
            _fixture: fixture,
        })
    }
}

struct TestHost(rivu::test_support::frontend::FrontendEndpoint);
impl Host for TestHost {
    fn display_state(&self) -> DisplayState {
        self.0.display_state()
    }
    fn display_state_if_changed(&self, revision: u64) -> Option<DisplayState> {
        self.0.display_state_if_changed(revision)
    }
    fn config_snapshot(&self) -> Arc<Config> {
        self.0.config_snapshot()
    }
    fn library_revision(&self) -> u64 {
        self.0.library_revision()
    }
    fn send(&self, command: Command) -> Result<()> {
        self.0.send(command)
    }
    fn request_ack(&self, command: Command) -> BoxFuture<'_, Ack> {
        self.0.request_ack(command)
    }
    fn query(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        timeout: Option<QueryTimeout>,
    ) -> BoxFuture<'_, QueryResponse> {
        self.0.query(query, ticket, timeout)
    }
    fn update_gui_config(&self, change: ConfigChange) -> Result<()> {
        self.0.update_gui_config(change)
    }
    fn apply_gui_settings(&self, patch: ConfigPatch) -> BoxFuture<'_, Ack> {
        self.0.apply_gui_settings(patch)
    }
    fn analysis(&self) -> &Arc<RwLock<AnalysisFrame>> {
        self.0.analysis()
    }
    fn waveform(&self) -> &Arc<RwLock<WaveformFrame>> {
        self.0.waveform()
    }
    fn set_display_demand(&self, demand: DisplayDemand) {
        self.0.set_display_demand(demand)
    }
    fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        self.0.analysis_frame_is_current(frame)
    }
    fn request_analysis_frame(&self) {
        self.0.request_analysis_frame()
    }
    fn prepare_waveform_scan(
        &self,
        parameters: rivu_protocol::waveform::WaveformScanParameters,
        selected: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<WaveformScan> {
        self.0
            .prepare_waveform_scan(parameters, selected, cancelled)
    }
    fn set_playback_wakeup(&self, callback: Wakeup) {
        self.0.set_playback_wakeup(callback)
    }
    fn clear_wakeup(&self) {
        self.0.clear_wakeup()
    }
    fn request_shutdown(&self) -> Result<()> {
        self.0.request_shutdown()
    }
    fn is_shutting_down(&self) -> bool {
        self.0.is_shutting_down()
    }
    fn set_control_wakeup(&self, callback: Wakeup) {
        self.0.set_control_wakeup(callback)
    }
    fn withdraw_gui(&self) {
        self.0.withdraw_gui()
    }
    fn take_raise_request(&self) -> bool {
        self.0.take_raise_request()
    }
    fn take_quit_request(&self) -> bool {
        self.0.take_quit_request()
    }
    fn raise_pending(&self) -> bool {
        self.0.raise_pending()
    }
    fn build_info(&self) -> BuildInfo {
        BuildInfo {
            version: "test",
            features: "test-support",
            timestamp: 0,
        }
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

pub(crate) fn artwork_track(path: std::path::PathBuf) -> rivu_protocol::model::Track {
    rivu_protocol::model::Track {
        id: 1,
        path,
        fingerprint: Some("original".into()),
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
