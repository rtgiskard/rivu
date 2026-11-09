//! Existing executable composition; the GUI library never starts a service.
use super::frontend_host::{absolute, ensure_service};
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

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
    let instance = match expected {
        Some(instance) => instance,
        None => async_io::block_on(ensure_service(&data_dir, &config_path))?,
    };
    // Service-originated desktops must verify that original owner without
    // bootstrapping a replacement. Ordinary launches reuse bootstrap's Hello;
    // only a scan needs another, independently identity-bound session.
    if expected.is_some() || !paths.is_empty() {
        let mut session = rivu_client::client_session(&data_dir);
        session.bind_instance(instance)?;
        if paths.is_empty() {
            session.get_state(rivu_protocol::response::StateSections::default())?;
        } else {
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
        let directory = crate::test_support::ipc::socket_test_directory();
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
        let mut registration = rivu_client::client_session(directory.path());
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
}
