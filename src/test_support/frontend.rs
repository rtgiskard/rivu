//! Genuine Core and IPC services for frontend integration tests.
use crate::{app::AppHandle, core::CoreRuntime, ipc};
use anyhow::Result;
use rivu_format::config::Config;
use std::path::Path;

/// Runs the same command and two PCM services as the application, without
/// process-wide logging, signals, desktop launching or desktop-bus services.
pub struct FrontendFixture {
    server: Option<ipc::Server>,
    audio: Option<ipc::audio::AudioServer>,
    pcm: Option<ipc::pcm::PcmServer>,
    // The runtime owns the executor that drains command and audio sessions.
    runtime: CoreRuntime,
    // Storage authority outlives every service, including partial teardown.
    _owner: ipc::SocketOwner,
}

impl FrontendFixture {
    pub fn start(data_dir: &Path, config: &Path) -> Result<Self> {
        let owner = ipc::SocketOwner::acquire(data_dir)?;
        let runtime = CoreRuntime::start(data_dir, config)?;
        let app = AppHandle::new(&runtime, data_dir);
        let pcm =
            ipc::pcm::PcmServer::start(data_dir, runtime.handle.clone(), owner.instance_id())?;
        let audio = ipc::audio::AudioServer::start(
            data_dir,
            runtime.handle.clone(),
            owner.instance_id(),
            runtime.test_executor(),
            runtime.test_workers(),
        )?;
        let server =
            ipc::Server::start(&owner, runtime.handle.clone(), app, runtime.test_executor())?;
        // Hello binds identity; a query also waits for Core's dispatch startup.
        rivu_client::client_session(data_dir)
            .query(rivu_protocol::model::Query::ScanTask { id: 0 })?;
        Ok(Self {
            server: Some(server),
            audio: Some(audio),
            pcm: Some(pcm),
            runtime,
            _owner: owner,
        })
    }

    pub fn executor(&self) -> &std::sync::Arc<async_executor::Executor<'static>> {
        self.runtime.test_executor()
    }
}

impl Drop for FrontendFixture {
    fn drop(&mut self) {
        // Cancel transport-owned work while Core's executor is still running.
        drop(self.audio.take());
        drop(self.pcm.take());
        drop(self.server.take());
        let _ = self.runtime.control.request_shutdown();
        let _ = self.runtime.join();
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
