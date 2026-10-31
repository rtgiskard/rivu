use super::*;

/// One optional startup read; dropping its waiter never detaches physical work.
pub(super) struct InitialDevices {
    pub(super) finished: oneshot::Receiver<Result<Vec<String>>>,
    worker: Option<async_executor::Task<()>>,
}

impl InitialDevices {
    pub(super) fn start(
        executor: &async_executor::Executor<'static>,
        workers: Arc<BlockingExecutor>,
        enumerate: impl FnOnce() -> Result<Vec<String>> + Send + 'static,
    ) -> Self {
        let (completed, finished) = oneshot::channel();
        let worker = executor.spawn(async move {
            let result = workers.execute(enumerate).await.and_then(|result| result);
            // One completion slot cannot backpressure shutdown retirement.
            let _ = completed.send(result);
        });
        Self {
            finished,
            worker: Some(worker),
        }
    }

    pub(super) fn join(&mut self) -> Result<()> {
        retire_optional_worker(
            &mut self.worker,
            "Initial device enumeration worker panicked",
        )
    }
}

impl Drop for InitialDevices {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// Runtime lifecycle control, separate from business requests and view wakeups.
#[derive(Clone)]
pub(crate) struct CoreLifecycle {
    shutdown: mpsc::Sender<()>,
    completion: Arc<Completion>,
}

#[derive(Default)]
struct Completion {
    state: Mutex<CompletionState>,
    acknowledged: Event,
}

#[derive(Default)]
struct CompletionState {
    ack: Option<Ack>,
    finished: bool,
    callback: Option<Wakeup>,
    error: Option<anyhow::Error>,
}

impl CoreLifecycle {
    pub(super) fn new() -> (Self, mpsc::Receiver<()>) {
        let (shutdown, receiver) = async_channel::bounded(1);
        (
            Self {
                shutdown,
                completion: Arc::new(Completion::default()),
            },
            receiver,
        )
    }

    pub(crate) fn request_shutdown(&self) -> Result<()> {
        match self.shutdown.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => Ok(()),
            Err(mpsc::TrySendError::Closed(())) => {
                anyhow::bail!("Rivu is stopped")
            }
        }
    }

    /// Acknowledges the shutdown request, not completion of runtime teardown.
    pub(crate) async fn shutdown_ack_async(&self, revision: impl Fn() -> u64) -> Ack {
        let requested = self.request_shutdown();
        let waiting = async {
            loop {
                let notified = self.completion.acknowledged.listen();
                {
                    let state = self.completion.state.lock();
                    if state.ack.is_some() || state.finished || requested.is_err() {
                        return;
                    }
                }
                notified.await;
            }
        };
        let _ = rivu_protocol::transport::timeout(REPLY_TIMEOUT, waiting).await;
        self.completion.state.lock().ack.clone().unwrap_or_else(|| {
            let status = if requested.is_err() {
                AckStatus::NotAdmitted
            } else {
                AckStatus::Unknown
            };
            Ack::new(
                status,
                Some(if status == AckStatus::NotAdmitted {
                    "Core stopped; shutdown request not admitted".into()
                } else {
                    "Core shutdown outcome unknown; do not automatically retry".into()
                }),
                revision(),
            )
        })
    }

    pub(super) fn acknowledge(&self, ack: Ack) {
        self.completion.state.lock().ack = Some(ack);
        self.completion.acknowledged.notify(usize::MAX);
    }

    pub(crate) fn set_completion_callback(&self, callback: impl Fn() + Send + Sync + 'static) {
        let callback: Wakeup = Arc::new(callback);
        let mut state = self.completion.state.lock();
        state.callback = Some(Arc::clone(&callback));
        let finished = state.finished;
        drop(state);
        if finished {
            callback();
        }
    }

    pub(super) fn record_result(&self, result: Result<()>) {
        self.completion.state.lock().error = result.err();
    }

    #[cfg(test)]
    pub(super) fn is_complete(&self) -> bool {
        self.completion.state.lock().finished
    }

    pub(super) fn take_error(&self) -> Option<anyhow::Error> {
        self.completion.state.lock().error.take()
    }

    fn complete(&self) {
        let mut state = self.completion.state.lock();
        state.finished = true;
        let callback = state.callback.clone();
        drop(state);
        self.completion.acknowledged.notify(usize::MAX);
        if let Some(callback) = callback {
            callback();
        }
    }
}

// Runs on normal return and panic, including failure before Core::run starts.
pub(super) struct CompletionGuard(pub(super) CoreLifecycle);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.complete();
    }
}

pub struct CoreRuntime {
    pub handle: CoreHandle,
    pub(crate) control: CoreLifecycle,
    worker: Option<JoinHandle<()>>,
    // Standalone test fixtures own execution; production ownership stays in App.
    #[cfg(test)]
    _test_tasks: Option<TaskRuntime>,
}
impl CoreRuntime {
    #[cfg(test)]
    pub(crate) fn test_executor(&self) -> &Arc<async_executor::Executor<'static>> {
        &self
            ._test_tasks
            .as_ref()
            .expect("standalone test runtime")
            .handle
    }

    #[cfg(test)]
    pub(crate) fn test_workers(&self) -> &Arc<BlockingExecutor> {
        &self
            ._test_tasks
            .as_ref()
            .expect("standalone test runtime")
            .workers
    }

    /// Load host configuration only when its path can be published over IPC.
    pub(crate) fn load_startup_config(config_path: &Path) -> Result<Config> {
        config_path
            .to_str()
            .context("Configuration path is not valid UTF-8; IPC requires UTF-8 paths")?;
        Config::load(config_path)
    }

    #[cfg(test)]
    /// Start a standalone core without installing or owning process logging.
    pub fn start(data_dir: &Path, config_path: &Path) -> Result<Self> {
        let tasks = TaskRuntime::start()?;
        let mut runtime = Self::start_inner(
            data_dir,
            config_path,
            Self::load_startup_config(config_path)?,
            None,
            Arc::clone(&tasks.handle),
            Arc::clone(&tasks.workers),
        )?;
        runtime._test_tasks = Some(tasks);
        Ok(runtime)
    }

    // App loads through load_startup_config before configuring file logging.
    pub(crate) fn start_with_logging(
        data_dir: &Path,
        config_path: &Path,
        config: Config,
        logging: rivu_utils::logging::Control,
        executor: Arc<async_executor::Executor<'static>>,
        workers: Arc<BlockingExecutor>,
    ) -> Result<Self> {
        Self::start_inner(
            data_dir,
            config_path,
            config,
            Some(logging),
            executor,
            workers,
        )
    }

    fn start_inner(
        data_dir: &Path,
        config_path: &Path,
        mut config: Config,
        logging: Option<rivu_utils::logging::Control>,
        executor: Arc<async_executor::Executor<'static>>,
        workers: Arc<BlockingExecutor>,
    ) -> Result<Self> {
        config.library_roots = config
            .library_roots
            .iter()
            .map(|root| library::logical_path(root))
            .collect::<Result<_>>()?;
        config.validate()?;
        // An existing file establishes an effective, normalized baseline without
        // rewriting user formatting or materializing omitted defaults at startup.
        let saved_config = config_path.try_exists()?.then(|| config.clone());
        std::fs::create_dir_all(data_dir)?;
        tracing::info!(path = %config_path.display(), "configuration_loaded");
        // Keep the handle usable with a config-only snapshot. Core loads its
        // database projections independently of optional device enumeration,
        // which runs through the application's bounded blocking executor.
        let store = Store::open(&data_dir.join("library.db"))?;
        store.set_track_cache_query_batch_size(config.query_batch_size);
        tracing::debug!("store_ready");
        let engine = AudioEngine::new(config.media_read_buffer_mib)?;
        tracing::info!("audio_engine_ready");
        let (sender, receiver) = async_channel::bounded(64);
        let (control, shutdown_receiver) = CoreLifecycle::new();
        let shared = Arc::new(RwLock::new(CoreState::default()));
        let wakeup = Arc::new(RwLock::new(None));
        let initial = {
            let mut state = CoreState::default();
            state.playback.volume = config.volume;
            state.playback.shuffle = config.shuffle;
            state.playback.repeat = config.repeat;
            state.system.selected_device = config.output_device.clone();
            state.configuration.config = Arc::new(config);
            state.configuration.config_path = config_path.to_path_buf();
            state
        };
        *shared.write() = initial.clone();
        let revision_updates = Arc::new(RwLock::new(Vec::new()));
        let task_waiters = Arc::new(Mutex::new(TaskWaiters::default()));
        let handle = CoreHandle {
            state: shared.clone(),
            analysis: engine.analysis.clone(),
            analysis_control: Arc::clone(&engine.analysis_control),
            waveform: Arc::clone(&engine.waveform),
            sender,
            revision_updates: Arc::clone(&revision_updates),
            wakeup: wakeup.clone(),
            task_waiters: Arc::clone(&task_waiters),
            query_timeout: None,
        };
        let (scan_tx, scan_rx) = async_channel::bounded(3);
        let (relocation_tx, relocation_rx) = async_channel::bounded(1);
        let core = Core {
            store,
            engine,
            logging,
            executor: executor.clone(),
            workers,
            read_queries: None,
            initial_devices: None,
            revision_updates,
            state: initial,
            shared,
            wakeup,
            task_waiters,
            task_completions_pending: false,
            generation: 0,
            output_settings_request: 0,
            next_playlist_token: 0,
            database_unavailable: false,
            queue_projection_stale: false,
            queue_current_pending: false,
            playback: None,
            retired_playback: None,
            playback_write_deferred: false,
            playback_history_pending: false,
            pending_bitrate: None,
            pending_navigation: None,
            active_export: None,
            active_external_admission: None,
            active_optimization: None,
            processing: crate::processing::Service::new(),
            pending_processing_audition: None,
            processing_results: VecDeque::new(),
            scan_tx,
            scan_rx,
            active_scan: None,
            scan_results: VecDeque::new(),
            next_scan_id: 1,
            relocation_tx,
            relocation_rx,
            active_relocation: None,
            relocation_results: VecDeque::new(),
            next_relocation_id: 1,
            played: Vec::new(),
            played_cursor: 0,
            saved_config,
            config_save_error: None,
            #[cfg(test)]
            _test_tasks: None,
        };
        let worker_control = control.clone();
        let worker = thread::Builder::new()
            .name("rivu-core".into())
            .spawn(move || {
                let _completion = CompletionGuard(worker_control.clone());
                // Local ownership ensures unwind drops Core before publishing
                // completion, on this thread rather than the shared reactor.
                let mut core = core;
                core.initialize();
                core.run(receiver, shutdown_receiver, &worker_control);
            })?;
        Ok(Self {
            handle,
            control,
            worker: Some(worker),
            #[cfg(test)]
            _test_tasks: None,
        })
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
    pub fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Rivu core worker panicked"))?;
        }
        // Only expose finalization failures after Core and its owned resources
        // have physically retired; shutdown acknowledgement remains admission.
        match self.control.take_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl Drop for CoreRuntime {
    fn drop(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            let _ = self.control.request_shutdown();
        }
        let _ = self.join();
    }
}
