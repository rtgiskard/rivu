//! Existing executable composition; the GUI library never starts a service.
use anyhow::{Context, Result, ensure};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
/// Wait for a real Hello, not merely a bound socket or a live child PID.
fn ensure_service(data_dir: &Path, config_path: &Path) -> Result<()> {
    let needs_start =
        match rivu_client::get_state(data_dir, rivu_protocol::response::StateSections::default()) {
            Ok(_) => return Ok(()),
            Err(error) => crate::ipc::is_no_instance(&error),
        };
    let mut child = if needs_start {
        let mut command = std::process::Command::new(
            std::env::current_exe().context("Locating Rivu service executable")?,
        );
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--config")
            .arg(config_path)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // The service must survive graphical caller and terminal session exit.
        // SAFETY: setsid is async-signal-safe and captures no parent state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        Some(
            command
                .spawn()
                .context("Starting independent Rivu service")?,
        )
    } else {
        // A host may own storage but still be initializing its IPC services.
        // Do not start a rival merely because its first Hello is waiting.
        None
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exited = None;
    let ready = loop {
        match rivu_client::get_state(data_dir, rivu_protocol::response::StateSections::default()) {
            Ok(_) => break Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    break Err(error).context("Waiting for service Hello readiness");
                }
                if exited.is_none()
                    && let Some(child) = child.as_mut()
                {
                    match child.try_wait() {
                        Ok(status) => exited = status,
                        Err(wait_error) => {
                            break Err(wait_error).context("Observing service startup");
                        }
                    }
                }
                // Duplicate children may lose ownership to another starter.
                // Successful Hello, not child ownership, decides who won.
                if let Some(status) = exited
                    && crate::ipc::is_no_instance(&error)
                {
                    break Err(error).with_context(|| {
                        format!("Rivu service exited before readiness ({status})")
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if exited.is_none()
        && let Some(mut child) = child
    {
        // Reap independently without tying service lifetime to GUI exit.
        std::thread::Builder::new()
            .name("rivu-service-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            })?;
    }
    ready
}

pub(super) fn open(data_dir: PathBuf, config_path: PathBuf, paths: Vec<PathBuf>) -> Result<()> {
    let data_dir = absolute(data_dir)?;
    let config_path = absolute(config_path)?;
    let expected = std::env::var_os("RIVU_DESKTOP_INSTANCE")
        .map(|value| {
            value
                .to_str()
                .context("Invalid desktop instance")?
                .parse::<u32>()
                .context("Invalid desktop instance")
        })
        .transpose()?;
    if expected.is_none() {
        ensure_service(&data_dir, &config_path)?;
    }
    let mut session = rivu_client::client_session(&data_dir)?;
    if let Some(expected) = expected {
        session.bind_instance(expected)?;
    }
    session.get_state(rivu_protocol::response::StateSections::default())?;
    let instance = session
        .instance_id()
        .context("GUI Hello did not bind a service")?;
    session.bind_instance(instance)?;
    if !paths.is_empty() {
        let ack = session.request_ack(rivu_protocol::model::Command::Scan {
            paths,
            force: false,
        })?;
        ensure!(
            ack.is_success(),
            "{}",
            ack.error.as_deref().unwrap_or("Scan failed")
        );
    }
    rivu_gui::run(data_dir, instance, super::build_info())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_originated_gui_never_bootstraps_a_replacement() {
        const CHILD_DIRECTORY: &str = "RIVU_TEST_NO_BOOTSTRAP_DIRECTORY";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            let directory = PathBuf::from(directory);
            assert!(
                crate::app::App::open_gui(
                    directory.clone(),
                    directory.join("config.toml"),
                    Vec::new()
                )
                .is_err()
            );
            assert!(crate::ipc::SocketOwner::acquire(&directory).is_ok());
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::desktop_host::tests::service_originated_gui_never_bootstraps_a_replacement",
            ])
            .env(CHILD_DIRECTORY, directory.path())
            .env("RIVU_DESKTOP_INSTANCE", "7")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn service_can_activate_after_desktop_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let owner = crate::ipc::SocketOwner::acquire(directory.path()).unwrap();
        let mut runtime = crate::core::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let app = crate::app::AppHandle::new(&runtime, directory.path());
        let server = crate::ipc::Server::start(
            &owner,
            runtime.handle.clone(),
            app.clone(),
            runtime.test_executor(),
        )
        .unwrap();
        assert!(app.can_raise());
        let mut registration = rivu_client::client_session(directory.path()).unwrap();
        assert!(registration.register_desktop().unwrap().is_success());
        assert!(app.can_raise());
        drop(registration);
        assert!(app.can_raise());
        assert!(
            rivu_client::get_state(
                directory.path(),
                rivu_protocol::response::StateSections::ALL
            )
            .is_ok()
        );
        app.request_shutdown().unwrap();
        assert!(!app.can_raise());
        runtime.join().unwrap();
        drop(server);
    }

    #[test]
    fn ready_service_does_not_validate_unused_desktop_config() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let directory = tempfile::tempdir().unwrap();
        let owner = crate::ipc::SocketOwner::acquire(directory.path()).unwrap();
        let mut runtime = crate::core::CoreRuntime::start(
            directory.path(),
            &directory.path().join("config.toml"),
        )
        .unwrap();
        let app = crate::app::AppHandle::new(&runtime, directory.path());
        let server = crate::ipc::Server::start(
            &owner,
            runtime.handle.clone(),
            app.clone(),
            runtime.test_executor(),
        )
        .unwrap();
        let unused_config = directory
            .path()
            .join(OsString::from_vec(b"unused-\xff.toml".to_vec()));
        let original = b"intentionally not valid TOML";
        std::fs::write(&unused_config, original).unwrap();
        ensure_service(directory.path(), &unused_config).unwrap();
        assert_eq!(std::fs::read(unused_config).unwrap(), original);
        app.request_shutdown().unwrap();
        runtime.join().unwrap();
        drop(server);
    }
}
