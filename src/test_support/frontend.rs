//! Genuine Core and catalog fixtures exposed only by the test-support feature.
use crate::{
    app::AppHandle,
    core::{CoreHandle, CoreRuntime},
    execution::BlockingExecutor,
};
use anyhow::Result;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use rivu_protocol::{
    analysis::{AnalysisFrame, DisplayDemand},
    display::DisplayState,
    model::{Command, Config, ConfigChange, ConfigPatch, Query, Track},
    query_tracking::QueryTicket,
    response::{Ack, QueryResponse},
    waveform::{WaveformFrame, WaveformScanParameters},
};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

type Wakeup = Arc<dyn Fn() + Send + Sync>;
type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;
pub type WaveformScan = Box<dyn FnOnce(Track) -> BoxFuture<'static, Result<WaveformFrame>> + Send>;

pub struct FrontendFixture {
    pub endpoint: FrontendEndpoint,
    _runtime: CoreRuntime,
}

impl FrontendFixture {
    pub fn start(data_dir: &Path, config: &Path) -> Result<Self> {
        let runtime = CoreRuntime::start(data_dir, config)?;
        let endpoint = FrontendEndpoint {
            core: runtime.handle.clone(),
            app: AppHandle::new(&runtime, true),
            workers: Arc::clone(runtime.test_workers()),
        };
        Ok(Self {
            endpoint,
            _runtime: runtime,
        })
    }
}

#[derive(Clone)]
pub struct FrontendEndpoint {
    core: CoreHandle,
    app: AppHandle,
    workers: Arc<BlockingExecutor>,
}
impl FrontendEndpoint {
    pub fn display_state(&self) -> DisplayState {
        self.core.display_state()
    }
    pub fn display_state_if_changed(&self, revision: u64) -> Option<DisplayState> {
        self.core.display_state_if_changed(revision)
    }
    pub fn config_snapshot(&self) -> Arc<Config> {
        self.core.config_snapshot()
    }
    pub fn library_revision(&self) -> u64 {
        self.core.library_revision()
    }
    pub fn send(&self, command: Command) -> Result<()> {
        self.core.send(command)
    }
    pub fn request_ack(&self, command: Command) -> BoxFuture<'_, Ack> {
        Box::pin(self.core.request_ack(command))
    }
    pub fn query(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        timeout: Option<QueryTimeout>,
    ) -> BoxFuture<'_, QueryResponse> {
        let mut core = self.core.clone();
        if let Some(timeout) = timeout {
            core = core.with_query_timeout(move |response| timeout(response));
        }
        Box::pin(async move {
            match ticket {
                Some(ticket) => core.query_if_current(query, ticket).await,
                None => core.query_for_gui(query).await,
            }
        })
    }
    pub fn update_gui_config(&self, change: ConfigChange) -> Result<()> {
        self.core.update_gui_config(change)
    }
    pub fn apply_gui_settings(&self, patch: ConfigPatch) -> BoxFuture<'_, Ack> {
        Box::pin(self.core.apply_gui_settings(patch))
    }
    pub fn analysis(&self) -> &Arc<RwLock<AnalysisFrame>> {
        &self.core.analysis
    }
    pub fn waveform(&self) -> &Arc<RwLock<WaveformFrame>> {
        &self.core.waveform
    }
    pub fn set_display_demand(&self, demand: DisplayDemand) {
        self.core.set_display_demand(demand);
    }
    pub fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        self.core.analysis_frame_is_current(frame)
    }
    pub fn request_analysis_frame(&self) {
        self.core.request_analysis_frame();
    }
    pub fn prepare_waveform_scan(
        &self,
        parameters: WaveformScanParameters,
        selected: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<WaveformScan> {
        self.core
            .prepare_waveform_scan(parameters, selected, cancelled, Arc::clone(&self.workers))
    }
    pub fn set_playback_wakeup(&self, callback: Wakeup) {
        self.core.set_wakeup(move || callback());
    }
    pub fn clear_wakeup(&self) {
        self.core.clear_wakeup();
    }
    pub fn request_shutdown(&self) -> Result<()> {
        self.app.request_shutdown()
    }
    pub fn is_shutting_down(&self) -> bool {
        self.app.is_shutting_down()
    }
    pub fn set_control_wakeup(&self, callback: Wakeup) {
        self.app.set_wakeup(move || callback());
    }
    pub fn withdraw_gui(&self) {
        self.app.withdraw_gui();
    }
    pub fn take_raise_request(&self) -> bool {
        self.app.take_raise_request()
    }
    pub fn take_quit_request(&self) -> bool {
        self.app.take_quit_request()
    }
    pub fn raise_pending(&self) -> bool {
        self.app.state.lock().raise_pending
    }
}

pub fn save_config(config: &Config, path: &Path) -> Result<()> {
    use crate::config::ConfigPersistence;
    config.save(path)
}

pub fn load_config(path: &Path) -> Result<Config> {
    use crate::config::ConfigPersistence;
    Config::load(path)
}
