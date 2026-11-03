//! In-process ports implemented by the application owner, independent of GPUI.
use anyhow::Result;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use rivu_protocol::{
    analysis::{AnalysisFrame, DisplayDemand},
    display::DisplayState,
    model::{Command, Config, ConfigChange, ConfigPatch, Query, Track},
    query_tracking::QueryTicket,
    response::{Ack, QueryResponse},
    waveform::WaveformFrame,
};
use std::{
    ops::Deref,
    sync::{Arc, atomic::AtomicBool},
};

pub type Wakeup = Arc<dyn Fn() + Send + Sync>;
pub type QueryTimeout = Arc<dyn Fn(QueryResponse) + Send + Sync>;
pub type WaveformScan = Box<dyn FnOnce(Track) -> BoxFuture<'static, Result<WaveformFrame>> + Send>;

#[derive(Clone, Copy, Debug)]
pub struct BuildInfo {
    pub version: &'static str,
    pub features: &'static str,
    pub timestamp: i64,
}

/// The GUI borrows published state and submits typed business operations. The
/// application retains every engine, database, executor and service owner.
pub trait Host: Send + Sync + 'static {
    fn display_state(&self) -> DisplayState;
    fn display_state_if_changed(&self, revision: u64) -> Option<DisplayState>;
    fn config_snapshot(&self) -> Arc<Config>;
    fn library_revision(&self) -> u64;
    fn send(&self, command: Command) -> Result<()>;
    fn request_ack(&self, command: Command) -> BoxFuture<'_, Ack>;
    fn query(
        &self,
        query: Query,
        ticket: Option<QueryTicket>,
        timeout: Option<QueryTimeout>,
    ) -> BoxFuture<'_, QueryResponse>;
    fn update_gui_config(&self, change: ConfigChange) -> Result<()>;
    fn apply_gui_settings(&self, patch: ConfigPatch) -> BoxFuture<'_, Ack>;
    fn analysis(&self) -> &Arc<RwLock<AnalysisFrame>>;
    fn waveform(&self) -> &Arc<RwLock<WaveformFrame>>;
    fn set_display_demand(&self, demand: DisplayDemand);
    fn analysis_frame_is_current(&self, frame: &AnalysisFrame) -> bool;
    fn request_analysis_frame(&self);
    fn prepare_waveform_scan(
        &self,
        parameters: rivu_protocol::waveform::WaveformScanParameters,
        selected: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<WaveformScan>;
    fn set_playback_wakeup(&self, callback: Wakeup);
    fn clear_wakeup(&self);
    fn request_shutdown(&self) -> Result<()>;
    fn is_shutting_down(&self) -> bool;
    fn set_control_wakeup(&self, callback: Wakeup);
    fn withdraw_gui(&self);
    fn take_raise_request(&self) -> bool;
    fn take_quit_request(&self) -> bool;
    fn raise_pending(&self) -> bool;
    fn build_info(&self) -> BuildInfo;
}

#[derive(Clone)]
pub struct HostHandle {
    port: Arc<dyn Host>,
    timeout: Option<QueryTimeout>,
}

impl HostHandle {
    pub fn new(port: impl Host) -> Self {
        Self {
            port: Arc::new(port),
            timeout: None,
        }
    }
    pub(crate) fn gui_snapshot(&self) -> crate::projection::GuiSnapshot {
        self.port.display_state().into()
    }
    pub(crate) fn gui_snapshot_if_changed(
        &self,
        revision: u64,
    ) -> Option<crate::projection::GuiSnapshot> {
        self.port.display_state_if_changed(revision).map(Into::into)
    }
    pub fn with_query_timeout(
        mut self,
        notify: impl Fn(QueryResponse) + Send + Sync + 'static,
    ) -> Self {
        self.timeout = Some(Arc::new(notify));
        self
    }
    pub async fn query_for_gui(&self, query: Query) -> QueryResponse {
        self.port.query(query, None, self.timeout.clone()).await
    }
    pub async fn query_if_current(&self, query: Query, ticket: QueryTicket) -> QueryResponse {
        self.port
            .query(query, Some(ticket), self.timeout.clone())
            .await
    }
    pub fn set_wakeup(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.port.set_playback_wakeup(Arc::new(callback));
    }
    #[cfg(test)]
    pub(crate) fn query(&self, query: Query) -> QueryResponse {
        futures::executor::block_on(self.query_for_gui(query))
    }
}

impl Deref for HostHandle {
    type Target = dyn Host;
    fn deref(&self) -> &Self::Target {
        self.port.as_ref()
    }
}
