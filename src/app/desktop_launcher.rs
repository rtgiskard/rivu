use super::AppHandle;
use anyhow::{Context, Result};
use futures::{FutureExt, channel::oneshot};
use std::{path::PathBuf, sync::Arc, time::Duration};

/// One service-owned launcher, with one queued intent and one owned child.
/// Child exit and desktop registration are event-driven; no PID polling.
pub(crate) struct DesktopLauncher {
    stop: Option<oneshot::Sender<()>>,
    worker: Option<async_executor::Task<()>>,
}

enum Event {
    Stop,
    Exited(std::io::Result<std::process::ExitStatus>),
    Timeout,
    Registered,
    Activate,
}

impl DesktopLauncher {
    pub(crate) fn start(
        app: AppHandle,
        config: PathBuf,
        instance_id: u32,
        executor: &Arc<async_executor::Executor<'static>>,
    ) -> Self {
        let (stop, stopped) = oneshot::channel();
        let worker = executor.spawn(async move {
            let mut stopped = stopped.fuse();
            let changes = app.subscribe_desktop();
            let mut child: Option<async_process::Child> = None;
            let mut pending = false;
            let mut deadline = None;
            loop {
                let event = {
                    let exited = async {
                        match child.as_mut() {
                            Some(child) => child.status().await,
                            None => futures::future::pending().await,
                        }
                    }.fuse();
                    let timeout = async {
                        match deadline {
                            Some(deadline) => { async_io::Timer::at(deadline).await; }
                            None => futures::future::pending().await,
                        }
                    }.fuse();
                    futures::pin_mut!(exited, timeout);
                    futures::select_biased! {
                        _ = stopped => Event::Stop,
                        result = exited => Event::Exited(result),
                        _ = timeout => Event::Timeout,
                        event = changes.recv().fuse() => if event.is_ok() { Event::Registered } else { Event::Stop },
                        intent = app.activation_requests.recv().fuse() => if intent.is_ok() { Event::Activate } else { Event::Stop },
                    }
                };
                match event {
                    Event::Stop => break,
                    Event::Exited(result) => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "desktop_child_wait_failed");
                        } else if let Ok(status) = result && !status.success() {
                            tracing::warn!(%status, "desktop_child_failed");
                        }
                        child = None;
                        deadline = None;
                        // Failed startup is reported, not automatically replayed.
                        pending = false;
                    },
                    Event::Timeout => {
                        if let Some(child) = child.as_mut() {
                            let _ = child.kill();
                        }
                        deadline = None;
                        pending = false;
                        tracing::warn!("desktop_startup_timed_out");
                    },
                    Event::Registered | Event::Activate => {
                        if matches!(event, Event::Registered) && !pending { continue; }
                        if app.is_shutting_down() { break; }
                        let raised = rivu_gui::raise_async(&app.data_dir, instance_id, deadline).fuse();
                        futures::pin_mut!(raised);
                        let result = futures::select_biased! {
                            _ = stopped => break,
                            result = raised => result,
                        };
                        if app.is_shutting_down() { break; }
                        if matches!(result, Ok(true)) {
                            pending = false;
                            deadline = None;
                            child = None;
                            continue;
                        }
                        pending = true;
                        if child.is_none() {
                            match spawn_desktop(&app, &config, instance_id) {
                                Ok(started) => {
                                    child = Some(started);
                                    deadline = Some(std::time::Instant::now() + Duration::from_secs(30));
                                }
                                Err(error) => {
                                    pending = false;
                                    tracing::warn!(%error, "desktop_start_failed");
                                }
                            }
                        }
                    },
                }
            }
            app.activation_requests.close();
            if let Some(mut child) = child {
                let _ = child.kill();
                let _ = child.status().await;
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

fn spawn_desktop(
    app: &AppHandle,
    config: &std::path::Path,
    instance_id: u32,
) -> Result<async_process::Child> {
    async_process::Command::new(
        std::env::current_exe().context("Locating Rivu desktop executable")?,
    )
    .arg("--data-dir")
    .arg(app.data_dir.as_path())
    .arg("--config")
    .arg(config)
    .arg("gui")
    .env("RIVU_DESKTOP_INSTANCE", instance_id.to_string())
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .context("Starting Rivu desktop")
}

impl Drop for DesktopLauncher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take()
            && async_io::block_on(std::panic::AssertUnwindSafe(worker).catch_unwind()).is_err()
        {
            tracing::warn!("desktop_launcher_panicked");
        }
    }
}
