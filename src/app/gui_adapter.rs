//! Application implementation of the GUI's narrow in-process host port.
use super::AppHandle;
use crate::{core::CoreHandle, execution::BlockingExecutor};
use anyhow::Result;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use rivu_gui::host::*;
use rivu_protocol::{
    analysis::{AnalysisFrame, DisplayDemand},
    display::DisplayState,
    model::{Command, Config, ConfigChange, ConfigPatch, Query},
    query_tracking::QueryTicket,
    response::{Ack, QueryResponse},
    waveform::{WaveformFrame, WaveformScanParameters},
};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) struct GuiAdapter {
    core: CoreHandle,
    app: AppHandle,
    workers: Arc<BlockingExecutor>,
    build: BuildInfo,
}

impl GuiAdapter {
    pub(super) fn new(core: CoreHandle, app: AppHandle, workers: Arc<BlockingExecutor>) -> Self {
        Self {
            core,
            app,
            workers,
            build: BuildInfo {
                version: env!("RIVU_GIT_VERSION"),
                features: env!("RIVU_BUILD_FEATURES"),
                timestamp: env!("RIVU_BUILD_TIMESTAMP")
                    .parse()
                    .expect("build script emits Unix seconds"),
            },
        }
    }
}

impl Host for GuiAdapter {
    fn display_state(&self) -> DisplayState {
        self.core.display_state()
    }
    fn display_state_if_changed(&self, revision: u64) -> Option<DisplayState> {
        self.core.display_state_if_changed(revision)
    }
    fn config_snapshot(&self) -> Arc<Config> {
        self.core.config_snapshot()
    }
    fn library_revision(&self) -> u64 {
        self.core.library_revision()
    }
    fn send(&self, command: Command) -> Result<()> {
        self.core.send(command)
    }
    fn request_ack(&self, command: Command) -> BoxFuture<'_, Ack> {
        Box::pin(self.core.request_ack(command))
    }
    fn query(
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
    fn update_gui_config(&self, change: ConfigChange) -> Result<()> {
        self.core.update_gui_config(change)
    }
    fn apply_gui_settings(&self, patch: ConfigPatch) -> BoxFuture<'_, Ack> {
        Box::pin(self.core.apply_gui_settings(patch))
    }
    fn analysis(&self) -> &Arc<RwLock<AnalysisFrame>> {
        &self.core.analysis
    }
    fn waveform(&self) -> &Arc<RwLock<WaveformFrame>> {
        &self.core.waveform
    }
    fn set_display_demand(&self, demand: DisplayDemand) {
        self.core.set_display_demand(demand);
    }
    fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool {
        self.core.analysis_frame_is_current(frame)
    }
    fn request_analysis_frame(&self) {
        self.core.request_analysis_frame();
    }
    fn prepare_waveform_scan(
        &self,
        parameters: WaveformScanParameters,
        selected: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<WaveformScan> {
        self.core
            .prepare_waveform_scan(parameters, selected, cancelled, Arc::clone(&self.workers))
    }
    fn set_playback_wakeup(&self, callback: Wakeup) {
        self.core.set_wakeup(move || callback());
    }
    fn clear_wakeup(&self) {
        self.core.clear_wakeup();
    }
    fn request_shutdown(&self) -> Result<()> {
        self.app.request_shutdown()
    }
    fn is_shutting_down(&self) -> bool {
        self.app.is_shutting_down()
    }
    fn set_control_wakeup(&self, callback: Wakeup) {
        self.app.set_wakeup(move || callback());
    }
    fn withdraw_gui(&self) {
        self.app.withdraw_gui();
    }
    fn take_raise_request(&self) -> bool {
        self.app.take_raise_request()
    }
    fn take_quit_request(&self) -> bool {
        self.app.take_quit_request()
    }
    fn raise_pending(&self) -> bool {
        self.app.state.lock().raise_pending
    }
    fn build_info(&self) -> BuildInfo {
        self.build
    }
}
