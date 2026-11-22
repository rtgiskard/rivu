//! Genuine Core and IPC services for frontend integration tests.
use crate::{app::AppHandle, core::CoreRuntime, ipc};
use anyhow::Result;
use rivu_format::config::Config;
use std::{fs, path::Path};

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
    // Drop after the listener so every fork that inherits it also pins the inode.
    _directory_identity: fs::File,
}

impl FrontendFixture {
    pub fn start(data_dir: &Path, config: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)?;
        let directory_identity = fs::File::open(data_dir)?;
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
        // Hello binds identity. Task lookup also waits for Core's dispatch startup.
        rivu_client::client_session(data_dir)
            .lookup_task(rivu_protocol::model::TaskTarget::Scan { id: 0 })?;
        Ok(Self {
            server: Some(server),
            audio: Some(audio),
            pcm: Some(pcm),
            runtime,
            _owner: owner,
            _directory_identity: directory_identity,
        })
    }

    pub fn executor(&self) -> &std::sync::Arc<async_executor::Executor<'static>> {
        self.runtime.test_executor()
    }

    /// The Core instance every client of this fixture binds to.
    pub fn instance_id(&self) -> u32 {
        self._owner.instance_id()
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
