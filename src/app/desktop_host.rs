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
    use std::{sync::atomic::Ordering, time::Duration};

    #[test]
    fn service_originated_gui_never_bootstraps_a_replacement() {
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
        // Only this exact test runs in the child, before any service thread
        // reads the environment.
        unsafe { std::env::set_var("RIVU_DESKTOP_INSTANCE", "7") };
        let directory = crate::test_support::ipc::socket_test_directory();
        let error = crate::app::App::open_gui(
            directory.path().to_owned(),
            directory.path().join("config.toml"),
            Vec::new(),
        )
        .unwrap_err();
        // The original owner's absence is the failure, and no replacement was launched.
        assert!(rivu_client::is_no_instance(&error), "{error:#}");
        assert_eq!(
            super::super::frontend_host::BOOTSTRAP_LAUNCHES.load(Ordering::Relaxed),
            0
        );
        assert!(crate::ipc::SocketOwner::acquire(directory.path()).is_ok());
    }

    #[test]
    fn service_can_activate_after_desktop_disconnect() {
        // Retirement is observed from the client's drop-driven EOF; isolation
        // keeps a sibling test's fork from holding that descriptor open.
        if rivu_utils::test_support::run_in_subprocess(Duration::from_secs(15)) {
            return;
        }
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
        let changes = app.subscribe_desktop_changes();
        changes.try_recv().unwrap();
        let mut registration = rivu_client::client_session(directory.path());
        assert!(registration.register_desktop().unwrap().is_success());
        assert!(!registration.register_desktop().unwrap().is_success());
        changes.try_recv().unwrap();
        drop(registration);
        async_io::block_on(rivu_protocol::transport::timeout(
            Duration::from_secs(5),
            changes.recv(),
        ))
        .expect("server did not retire the disconnected desktop registration")
        .unwrap();
        let mut client = rivu_client::client_session(directory.path());
        assert!(
            client
                .request_ack(rivu_protocol::AppCommand::ShowWindow)
                .unwrap()
                .is_success()
        );
        assert_eq!(app.activation_requests.try_recv(), Ok(()));
        app.request_shutdown().unwrap();
        assert!(!app.can_raise());
        runtime.join().unwrap();
        drop(server);
    }
}
